use anyhow::{Context, Result};
use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
use std::path::Path;
use std::str::FromStr;

/// Initializes the SQLite database, creating the file if it does not exist,
/// and runs all embedded database migrations.
pub async fn init_db(db_path: &str) -> Result<SqlitePool> {
    // Create parent directories if they don't exist
    if let Some(parent) = Path::new(db_path).parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directories for DB path: {:?}", parent))?;
    }

    // Configure SQLite connection options to automatically create the DB if missing
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path))?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

    let pool = SqlitePool::connect_with(options)
        .await
        .context("Failed to connect to SQLite database")?;

    // ─── Pre-Migration Tasks Table Upgrade Check ────────────────────────────────────
    // If the database has an existing `tasks` table with the old schema (lacking `created_at`),
    // we rename it to `tasks_old` and create the base new `tasks` table (matching 20260620000000_tasks.sql).
    // The message_id column will be added subsequently by the 20260620000001 sqlx migration.
    let table_exists: (i32,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='tasks'",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or((0,));

    let mut migrated_tasks = false;

    if table_exists.0 > 0 {
        let has_created_at: (i32,) = sqlx::query_as(
            "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name='created_at'",
        )
        .fetch_one(&pool)
        .await
        .unwrap_or((0,));

        if has_created_at.0 == 0 {
            println!("Database: Pre-migration - Renaming tasks to tasks_old and preparing new tasks table...");
            let mut tx = pool.begin().await?;

            sqlx::query("ALTER TABLE tasks RENAME TO tasks_old;")
                .execute(&mut *tx)
                .await?;

            sqlx::query(
                "CREATE TABLE tasks (
                    id                TEXT PRIMARY KEY,
                    created_at        DATETIME NOT NULL,
                    updated_at        DATETIME NOT NULL,
                    title             TEXT NOT NULL,
                    description       TEXT,
                    assigned_to       TEXT,
                    due_date          TEXT,
                    duration_minutes  INTEGER NOT NULL DEFAULT 30,
                    priority          TEXT NOT NULL DEFAULT 'medium',
                    status            TEXT NOT NULL DEFAULT 'open',
                    calendar_event_id TEXT,
                    source            TEXT NOT NULL DEFAULT 'manual'
                );",
            )
            .execute(&mut *tx)
            .await?;

            tx.commit().await?;
            migrated_tasks = true;
        }
    }

    if table_exists.0 > 0 {
        // If the new tasks table was previously created with message_id in a failed run,
        // but the migration 20260620000001 has NOT run yet, we drop the column so the migration can apply.
        let has_message_id: (i32,) = sqlx::query_as(
            "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name='message_id'",
        )
        .fetch_one(&pool)
        .await
        .unwrap_or((0,));

        let migration_applied: (i32,) = sqlx::query_as(
            "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 20260620000001",
        )
        .fetch_one(&pool)
        .await
        .unwrap_or((0,));

        if has_message_id.0 > 0 && migration_applied.0 == 0 {
            println!("Database: Clean up duplicate message_id column for migration compatibility...");
            if sqlx::query("ALTER TABLE tasks DROP COLUMN message_id;")
                .execute(&pool)
                .await
                .is_err()
            {
                let mut tx = pool.begin().await?;
                let _ = sqlx::query("DROP INDEX IF EXISTS idx_tasks_message_id;")
                    .execute(&mut *tx)
                    .await;
                let _ = sqlx::query("ALTER TABLE tasks RENAME TO tasks_temp;")
                    .execute(&mut *tx)
                    .await;
                let _ = sqlx::query(
                    "CREATE TABLE tasks (
                        id                TEXT PRIMARY KEY,
                        created_at        DATETIME NOT NULL,
                        updated_at        DATETIME NOT NULL,
                        title             TEXT NOT NULL,
                        description       TEXT,
                        assigned_to       TEXT,
                        due_date          TEXT,
                        duration_minutes  INTEGER NOT NULL DEFAULT 30,
                        priority          TEXT NOT NULL DEFAULT 'medium',
                        status            TEXT NOT NULL DEFAULT 'open',
                        calendar_event_id TEXT,
                        source            TEXT NOT NULL DEFAULT 'manual'
                    );",
                )
                .execute(&mut *tx)
                .await;
                let _ = sqlx::query(
                    "INSERT INTO tasks (id, created_at, updated_at, title, description, assigned_to, due_date, duration_minutes, priority, status, calendar_event_id, source) \
                     SELECT id, created_at, updated_at, title, description, assigned_to, due_date, duration_minutes, priority, status, calendar_event_id, source \
                     FROM tasks_temp;",
                )
                .execute(&mut *tx)
                .await;
                let _ = sqlx::query("DROP TABLE tasks_temp;").execute(&mut *tx).await;
                tx.commit().await?;
            }
        }
    }

    run_migrations_resolving_tasks_message_id(&pool)
        .await
        .context("Failed to run database migrations")?;

    // ─── Post-Migration Data Migration ──────────────────────────────────────────────
    // If we renamed tasks to tasks_old, we now copy all the data over and drop the old table.
    if migrated_tasks {
        let has_tasks_old: (i32,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='tasks_old'",
        )
        .fetch_one(&pool)
        .await
        .unwrap_or((0,));

        if has_tasks_old.0 > 0 {
            println!("Database: Post-migration - Migrating tasks data and clean up tasks_old...");
            let mut tx = pool.begin().await?;

            let has_message_id: (i32,) = sqlx::query_as(
                "SELECT COUNT(*) FROM pragma_table_info('tasks_old') WHERE name='message_id'",
            )
            .fetch_one(&mut *tx)
            .await
            .unwrap_or((0,));

            if has_message_id.0 > 0 {
                sqlx::query(
                    "INSERT INTO tasks (id, created_at, updated_at, title, status, source, message_id) \
                     SELECT id, timestamp, timestamp, task_description, \
                            CASE WHEN status = 'completed' OR status = 'done' THEN 'done' ELSE 'open' END, \
                            'manual', message_id \
                     FROM tasks_old;",
                )
                .execute(&mut *tx)
                .await?;
            } else {
                sqlx::query(
                    "INSERT INTO tasks (id, created_at, updated_at, title, status, source) \
                     SELECT id, timestamp, timestamp, task_description, \
                            CASE WHEN status = 'completed' OR status = 'done' THEN 'done' ELSE 'open' END, \
                            'manual' \
                     FROM tasks_old;",
                )
                .execute(&mut *tx)
                .await?;
            }

            sqlx::query("DROP TABLE tasks_old;")
                .execute(&mut *tx)
                .await?;

            tx.commit().await?;
            println!("Database: Post-migration tasks migration completed successfully!");
        }
    }

    // Fresh DBs hit 20260607000000's legacy `tasks` CREATE first; later
    // `CREATE TABLE IF NOT EXISTS` migrations are no-ops. Rebuild to the modern
    // schema (including due_at / reminded_at) when `created_at` is still missing.
    ensure_modern_tasks_schema(&pool)
        .await
        .context("Failed to ensure modern tasks schema")?;

    backfill_memory_chunk_task_owners(&pool)
        .await
        .context("Failed to backfill memory chunk task owners")?;

    let tagged = crate::food_tags::backfill_food_log_keyword_tags(&pool)
        .await
        .context("Failed to backfill food_log keyword tags")?;
    if tagged > 0 {
        println!("Database: Keyword-tagged {tagged} historical food_log row(s).");
    }

    Ok(pool)
}

/// Run embedded migrations, dropping `tasks.message_id` before the second ADD
/// (`20260620000001`) so fresh databases that already received it from
/// `20260607000100` do not fail with "duplicate column name".
async fn run_migrations_resolving_tasks_message_id(pool: &SqlitePool) -> Result<()> {
    let migrator = sqlx::migrate!();

    // Stop after the first message_id ADD so we can drop it before the duplicate.
    migrator
        .run_to(20260607000100, pool)
        .await
        .context("Failed to run database migrations through 20260607000100")?;

    drop_tasks_message_id_if_present(pool).await?;

    migrator
        .run(pool)
        .await
        .context("Failed to run remaining database migrations")?;

    Ok(())
}

async fn drop_tasks_message_id_if_present(pool: &SqlitePool) -> Result<()> {
    let has_tasks: (i32,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='tasks'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or((0,));
    if has_tasks.0 == 0 {
        return Ok(());
    }

    let has_message_id: (i32,) = sqlx::query_as(
        "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name='message_id'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or((0,));
    if has_message_id.0 == 0 {
        return Ok(());
    }

    let migration_applied: (i32,) = sqlx::query_as(
        "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 20260620000001",
    )
    .fetch_one(pool)
    .await
    .unwrap_or((0,));
    if migration_applied.0 > 0 {
        return Ok(());
    }

    println!("Database: Dropping tasks.message_id before migration 20260620000001...");
    let _ = sqlx::query("DROP INDEX IF EXISTS idx_tasks_message_id;")
        .execute(pool)
        .await;

    if sqlx::query("ALTER TABLE tasks DROP COLUMN message_id;")
        .execute(pool)
        .await
        .is_ok()
    {
        return Ok(());
    }

    // Fallback: rebuild without message_id so the next ALTER can succeed.
    let has_created_at: (i32,) = sqlx::query_as(
        "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name='created_at'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or((0,));

    let mut tx = pool.begin().await?;
    if has_created_at.0 > 0 {
        sqlx::query("ALTER TABLE tasks RENAME TO tasks_temp;")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "CREATE TABLE tasks (
                id                TEXT PRIMARY KEY,
                created_at        DATETIME NOT NULL,
                updated_at        DATETIME NOT NULL,
                title             TEXT NOT NULL,
                description       TEXT,
                assigned_to       TEXT,
                due_date          TEXT,
                duration_minutes  INTEGER NOT NULL DEFAULT 30,
                priority          TEXT NOT NULL DEFAULT 'medium',
                status            TEXT NOT NULL DEFAULT 'open',
                calendar_event_id TEXT,
                source            TEXT NOT NULL DEFAULT 'manual'
            );",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO tasks (id, created_at, updated_at, title, description, assigned_to, due_date, duration_minutes, priority, status, calendar_event_id, source) \
             SELECT id, created_at, updated_at, title, description, assigned_to, due_date, duration_minutes, priority, status, calendar_event_id, source \
             FROM tasks_temp;",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("DROP TABLE tasks_temp;")
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query(
            "CREATE TABLE tasks_nomsg AS SELECT id, timestamp, task_description, status FROM tasks;",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("DROP TABLE tasks;").execute(&mut *tx).await?;
        sqlx::query(
            "CREATE TABLE tasks (
                id TEXT PRIMARY KEY,
                timestamp DATETIME NOT NULL,
                task_description TEXT NOT NULL,
                status TEXT NOT NULL
            );",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO tasks (id, timestamp, task_description, status) \
             SELECT id, timestamp, task_description, status FROM tasks_nomsg;",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("DROP TABLE tasks_nomsg;")
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Rebuild `tasks` when the legacy email-classification schema is still present.
///
/// On a brand-new database, `20260607000000_email_classifications.sql` creates
/// `tasks(id, timestamp, task_description, status)`. Subsequent
/// `CREATE TABLE IF NOT EXISTS tasks (...)` migrations do not replace it, so
/// runtime inserts that expect `title` / `created_at` / `due_at` would fail.
async fn ensure_modern_tasks_schema(pool: &SqlitePool) -> Result<()> {
    let table_exists: (i32,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='tasks'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or((0,));

    if table_exists.0 == 0 {
        return Ok(());
    }

    let has_created_at: (i32,) = sqlx::query_as(
        "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name='created_at'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or((0,));

    if has_created_at.0 > 0 {
        return Ok(());
    }

    println!(
        "Database: Legacy tasks schema detected after migrations — rebuilding to modern schema..."
    );

    let mut tx = pool.begin().await?;

    sqlx::query("DROP INDEX IF EXISTS idx_tasks_message_id;")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DROP INDEX IF EXISTS idx_tasks_due_at;")
        .execute(&mut *tx)
        .await?;
    sqlx::query("ALTER TABLE tasks RENAME TO tasks_legacy_schema;")
        .execute(&mut *tx)
        .await?;

    // Final runtime schema (base + later ALTER columns). Migrations already ran,
    // so this must include message_id / telegram fields / due_at / reminded_at.
    sqlx::query(
        "CREATE TABLE tasks (
            id                  TEXT PRIMARY KEY,
            created_at          DATETIME NOT NULL,
            updated_at          DATETIME NOT NULL,
            title               TEXT NOT NULL,
            description         TEXT,
            assigned_to         TEXT,
            due_date            TEXT,
            duration_minutes    INTEGER NOT NULL DEFAULT 30,
            priority            TEXT NOT NULL DEFAULT 'medium',
            status              TEXT NOT NULL DEFAULT 'open',
            calendar_event_id   TEXT,
            source              TEXT NOT NULL DEFAULT 'manual',
            message_id          TEXT,
            telegram_message_id INTEGER,
            email_sender        TEXT,
            email_subject       TEXT,
            due_at              TEXT,
            reminded_at         TEXT
        );",
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS idx_tasks_message_id ON tasks(message_id);")
        .execute(&mut *tx)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_tasks_due_at ON tasks(due_at);")
        .execute(&mut *tx)
        .await?;

    let legacy_cols: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM pragma_table_info('tasks_legacy_schema')")
            .fetch_all(&mut *tx)
            .await?;
    let legacy: std::collections::HashSet<String> =
        legacy_cols.into_iter().map(|(n,)| n).collect();

    let has = |c: &str| legacy.contains(c);
    let col = |c: &str, fallback: &str| -> String {
        if has(c) {
            c.to_string()
        } else {
            fallback.to_string()
        }
    };

    let title_expr = if has("title") {
        "title".to_string()
    } else if has("task_description") {
        "task_description".to_string()
    } else {
        "''".to_string()
    };
    let created_expr = if has("created_at") {
        "created_at".to_string()
    } else if has("timestamp") {
        "timestamp".to_string()
    } else {
        "CURRENT_TIMESTAMP".to_string()
    };
    let updated_expr = if has("updated_at") {
        "updated_at".to_string()
    } else {
        created_expr.clone()
    };
    let status_expr = if has("status") {
        "CASE WHEN status IN ('completed', 'done') THEN 'done' \
              WHEN status = 'snoozed' THEN 'snoozed' \
              WHEN status = 'ignored' THEN 'ignored' \
              ELSE 'open' END"
            .to_string()
    } else {
        "'open'".to_string()
    };

    let sql = format!(
        "INSERT INTO tasks (
            id, created_at, updated_at, title, description, assigned_to, due_date,
            duration_minutes, priority, status, calendar_event_id, source,
            message_id, telegram_message_id, email_sender, email_subject,
            due_at, reminded_at
         )
         SELECT
            id,
            {created},
            {updated},
            {title},
            {description},
            {assigned_to},
            {due_date},
            {duration_minutes},
            {priority},
            {status},
            {calendar_event_id},
            {source},
            {message_id},
            {telegram_message_id},
            {email_sender},
            {email_subject},
            {due_at},
            {reminded_at}
         FROM tasks_legacy_schema;",
        created = created_expr,
        updated = updated_expr,
        title = title_expr,
        description = col("description", "NULL"),
        assigned_to = col("assigned_to", "NULL"),
        due_date = col("due_date", "NULL"),
        duration_minutes = col("duration_minutes", "30"),
        priority = col("priority", "'medium'"),
        status = status_expr,
        calendar_event_id = col("calendar_event_id", "NULL"),
        source = col("source", "'manual'"),
        message_id = col("message_id", "NULL"),
        telegram_message_id = col("telegram_message_id", "NULL"),
        email_sender = col("email_sender", "NULL"),
        email_subject = col("email_subject", "NULL"),
        due_at = col("due_at", "NULL"),
        reminded_at = col("reminded_at", "NULL"),
    );

    // Column names come from pragma_table_info / allowlisted fallbacks only.
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .execute(&mut *tx)
        .await?;
    sqlx::query("DROP TABLE tasks_legacy_schema;")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    println!("Database: Modern tasks schema rebuild completed.");
    Ok(())
}

/// Copy `tasks.assigned_to` onto existing `memory_chunks` so linked-DM search
/// does not treat assigned tasks as unassigned (NULL owner) before a reindex.
///
/// Only fills NULL owners when a matching task has a non-NULL assignee, so
/// boot does not rewrite already-owned rows or NULL out orphans.
async fn backfill_memory_chunk_task_owners(pool: &SqlitePool) -> Result<()> {
    let has_owner: (i32,) = sqlx::query_as(
        "SELECT COUNT(*) FROM pragma_table_info('memory_chunks') WHERE name='owner_member_id'",
    )
    .fetch_one(pool)
    .await?;
    if has_owner.0 == 0 {
        return Ok(());
    }
    let has_assigned: (i32,) = sqlx::query_as(
        "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name='assigned_to'",
    )
    .fetch_one(pool)
    .await?;
    if has_assigned.0 == 0 {
        return Ok(());
    }

    sqlx::query(
        "UPDATE memory_chunks \
         SET owner_member_id = ( \
             SELECT assigned_to FROM tasks WHERE tasks.id = memory_chunks.source_id \
         ) \
         WHERE source_type = 'task' \
           AND owner_member_id IS NULL \
           AND EXISTS ( \
               SELECT 1 FROM tasks \
               WHERE tasks.id = memory_chunks.source_id \
                 AND tasks.assigned_to IS NOT NULL \
           )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// A task row returned after marking it done via [`complete_all_open_tasks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedTaskRow {
    pub id: String,
    pub title: String,
    pub calendar_event_id: Option<String>,
    pub assigned_to: Option<String>,
}

/// List open/snoozed tasks that would be completed by [`complete_all_open_tasks`].
///
/// When `assignee_filter` is `Some(member_id)`, only tasks assigned to that member
/// or unassigned (`assigned_to IS NULL`) are included — matching the task-list
/// member filter.
pub async fn list_completable_open_tasks(
    pool: &SqlitePool,
    assignee_filter: Option<&str>,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    let mut qb = sqlx::QueryBuilder::new(
        "SELECT id, title FROM tasks WHERE status IN ('open', 'snoozed')",
    );
    if let Some(member_id) = assignee_filter {
        qb.push(" AND (assigned_to = ");
        qb.push_bind(member_id);
        qb.push(" OR assigned_to IS NULL)");
    }
    qb.push(" ORDER BY due_date IS NULL, due_date ASC, created_at ASC");
    qb.build_query_as::<(String, String)>()
        .fetch_all(pool)
        .await
}

/// Mark open/snoozed tasks as done. Returns each completed task (empty when
/// there was nothing to complete), including any `calendar_event_id` that was
/// present at completion time.
///
/// Does **not** clear `calendar_event_id` — callers should delete the Google
/// event first, then clear the stored id only after a successful delete (or
/// confirmed 404) so a failed cleanup can be retried.
pub async fn complete_all_open_tasks(
    pool: &SqlitePool,
    now_rfc3339: &str,
    assignee_filter: Option<&str>,
) -> Result<Vec<CompletedTaskRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let rows = {
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT id, title, calendar_event_id, assigned_to FROM tasks \
             WHERE status IN ('open', 'snoozed')",
        );
        if let Some(member_id) = assignee_filter {
            qb.push(" AND (assigned_to = ");
            qb.push_bind(member_id);
            qb.push(" OR assigned_to IS NULL)");
        }
        qb.push(" ORDER BY due_date IS NULL, due_date ASC, created_at ASC");
        qb.build_query_as::<(String, String, Option<String>, Option<String>)>()
            .fetch_all(&mut *tx)
            .await?
    };

    if !rows.is_empty() {
        let mut qb = sqlx::QueryBuilder::new(
            "UPDATE tasks SET status = 'done', updated_at = ",
        );
        qb.push_bind(now_rfc3339);
        qb.push(" WHERE status IN ('open', 'snoozed')");
        if let Some(member_id) = assignee_filter {
            qb.push(" AND (assigned_to = ");
            qb.push_bind(member_id);
            qb.push(" OR assigned_to IS NULL)");
        }
        qb.build().execute(&mut *tx).await?;
    }

    tx.commit().await?;
    Ok(rows
        .into_iter()
        .map(|(id, title, calendar_event_id, assigned_to)| CompletedTaskRow {
            id,
            title,
            calendar_event_id,
            assigned_to,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn insert_task(pool: &SqlitePool, id: &str, title: &str, status: &str) {
        insert_task_assigned(pool, id, title, status, None).await;
    }

    async fn insert_task_assigned(
        pool: &SqlitePool,
        id: &str,
        title: &str,
        status: &str,
        assigned_to: Option<&str>,
    ) {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO tasks (id, created_at, updated_at, title, assigned_to, due_date, due_at, status, source) \
             VALUES (?, ?, ?, ?, ?, NULL, NULL, ?, 'manual')",
        )
        .bind(id)
        .bind(&now)
        .bind(&now)
        .bind(title)
        .bind(assigned_to)
        .bind(status)
        .execute(pool)
        .await
        .expect("insert task");
    }

    #[tokio::test]
    async fn fresh_db_tasks_has_modern_columns() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("fresh.db");
        let pool = init_db(db_path.to_str().unwrap()).await.unwrap();

        let cols: Vec<(String,)> = sqlx::query_as("SELECT name FROM pragma_table_info('tasks')")
            .fetch_all(&pool)
            .await
            .unwrap();
        let names: std::collections::HashSet<_> = cols.into_iter().map(|(n,)| n).collect();

        for required in [
            "created_at",
            "updated_at",
            "title",
            "due_date",
            "due_at",
            "reminded_at",
            "message_id",
            "status",
            "source",
        ] {
            assert!(
                names.contains(required),
                "fresh DB tasks missing column `{required}`; have {names:?}"
            );
        }

        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO tasks (id, created_at, updated_at, title, assigned_to, due_date, due_at, status, source) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 'open', 'manual')",
        )
        .bind("test-id")
        .bind(&now)
        .bind(&now)
        .bind("buy milk")
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .execute(&pool)
        .await
        .expect("manual task insert should succeed on fresh DB");
    }

    #[tokio::test]
    async fn condition_tracking_tables_and_tag_seed() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("conditions.db");
        let pool = init_db(db_path.to_str().unwrap()).await.unwrap();

        for table in [
            "food_tags",
            "food_log_tags",
            "condition_watchlist",
            "condition_checkin",
        ] {
            let exists: (i32,) = sqlx::query_as(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
            )
            .bind(table)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(exists.0, 1, "missing table {table}");
        }

        let tags: Vec<(String,)> = sqlx::query_as("SELECT tag FROM food_tags ORDER BY tag")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(tags.len(), 14);
        assert!(tags.iter().any(|(t,)| t == "alcohol"));
        assert!(tags.iter().any(|(t,)| t == "nightshades"));
    }

    #[tokio::test]
    async fn food_log_keyword_backfill_and_tag_delete() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("food_tags.db");
        let pool = init_db(db_path.to_str().unwrap()).await.unwrap();

        sqlx::query(
            "INSERT INTO food_log (id, timestamp, family_member_id, raw_text_description, estimated_calories) \
             VALUES ('log-beer', datetime('now'), 'alex', '2 beers and nachos', 500)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let n = crate::food_tags::backfill_food_log_keyword_tags(&pool)
            .await
            .unwrap();
        assert!(n >= 1);

        let tags: Vec<(String, String)> = sqlx::query_as(
            "SELECT tag, source FROM food_log_tags WHERE food_log_id = 'log-beer' ORDER BY tag",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(tags.iter().any(|(t, s)| t == "alcohol" && s == "keyword"));
        assert!(tags.iter().any(|(t, _)| t == "fried"));

        crate::food_tags::delete_food_log_tags(&pool, "log-beer")
            .await
            .unwrap();
        let left: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM food_log_tags WHERE food_log_id = 'log-beer'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(left.0, 0);
    }

    #[tokio::test]
    async fn complete_all_open_tasks_marks_open_and_snoozed_only() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("complete_all.db");
        let pool = init_db(db_path.to_str().unwrap()).await.unwrap();

        insert_task(&pool, "t-open", "change fob battery", "open").await;
        insert_task(&pool, "t-snooze", "call dentist", "snoozed").await;
        insert_task(&pool, "t-done", "already done", "done").await;
        insert_task(&pool, "t-ignored", "ignored mail", "ignored").await;

        let now = chrono::Utc::now().to_rfc3339();
        let preview = list_completable_open_tasks(&pool, None).await.unwrap();
        assert_eq!(preview.len(), 2);

        let completed = complete_all_open_tasks(&pool, &now, None).await.unwrap();
        assert_eq!(completed.len(), 2);
        let titles: std::collections::HashSet<_> =
            completed.iter().map(|r| r.title.as_str()).collect();
        assert!(titles.contains("change fob battery"));
        assert!(titles.contains("call dentist"));

        let statuses: Vec<(String, String)> =
            sqlx::query_as("SELECT id, status FROM tasks ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        let map: std::collections::HashMap<_, _> = statuses.into_iter().collect();
        assert_eq!(map["t-open"], "done");
        assert_eq!(map["t-snooze"], "done");
        assert_eq!(map["t-done"], "done");
        assert_eq!(map["t-ignored"], "ignored");

        // Second call is a no-op.
        let again = complete_all_open_tasks(&pool, &now, None).await.unwrap();
        assert!(again.is_empty());
    }

    #[tokio::test]
    async fn complete_all_open_tasks_respects_assignee_filter() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("complete_all_scoped.db");
        let pool = init_db(db_path.to_str().unwrap()).await.unwrap();

        insert_task_assigned(&pool, "t-alex", "alex task", "open", Some("alex")).await;
        insert_task_assigned(&pool, "t-jordan", "jordan task", "open", Some("jordan")).await;
        insert_task_assigned(&pool, "t-unassigned", "shared chore", "snoozed", None).await;

        let now = chrono::Utc::now().to_rfc3339();
        let completed = complete_all_open_tasks(&pool, &now, Some("alex"))
            .await
            .unwrap();
        let titles: std::collections::HashSet<_> =
            completed.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(completed.len(), 2);
        assert!(titles.contains("alex task"));
        assert!(titles.contains("shared chore"));
        assert!(!titles.contains("jordan task"));

        let statuses: Vec<(String, String)> =
            sqlx::query_as("SELECT id, status FROM tasks ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        let map: std::collections::HashMap<_, _> = statuses.into_iter().collect();
        assert_eq!(map["t-alex"], "done");
        assert_eq!(map["t-unassigned"], "done");
        assert_eq!(map["t-jordan"], "open");
    }

    #[tokio::test]
    async fn complete_all_open_tasks_returns_calendar_event_id_without_clearing() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("complete_all_cal.db");
        let pool = init_db(db_path.to_str().unwrap()).await.unwrap();

        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO tasks (id, created_at, updated_at, title, assigned_to, due_date, due_at, status, source, calendar_event_id) \
             VALUES (?, ?, ?, ?, ?, NULL, NULL, 'open', 'manual', ?)",
        )
        .bind("t-cal")
        .bind(&now)
        .bind(&now)
        .bind("dentist")
        .bind(Some("alex"))
        .bind(Some("evt-123"))
        .execute(&pool)
        .await
        .unwrap();

        let completed = complete_all_open_tasks(&pool, &now, None).await.unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].calendar_event_id.as_deref(), Some("evt-123"));
        assert_eq!(completed[0].assigned_to.as_deref(), Some("alex"));

        // Id is kept until the caller successfully deletes the Google event.
        let stored: Option<String> =
            sqlx::query_scalar("SELECT calendar_event_id FROM tasks WHERE id = 't-cal'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored.as_deref(), Some("evt-123"));
    }

    #[tokio::test]
    async fn backfill_copies_task_assignee_onto_memory_chunks() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("memory_owner_backfill.db");
        let pool = init_db(db_path.to_str().unwrap()).await.unwrap();

        insert_task_assigned(&pool, "t-alex", "alex task", "open", Some("alex")).await;
        insert_task_assigned(&pool, "t-open", "unassigned", "open", None).await;
        sqlx::query(
            "INSERT INTO memory_chunks \
             (id, source_type, source_id, title, body, owner_member_id, content_hash, embedding, updated_at) \
             VALUES \
             ('c1', 'task', 't-alex', 'alex task', 'body', NULL, 'h', x'00', 'now'), \
             ('c2', 'task', 't-open', 'unassigned', 'body', NULL, 'h', x'00', 'now'), \
             ('c3', 'task', 't-gone', 'orphan', 'body', 'jordan', 'h', x'00', 'now')",
        )
        .execute(&pool)
        .await
        .unwrap();

        backfill_memory_chunk_task_owners(&pool).await.unwrap();

        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT source_id, owner_member_id FROM memory_chunks ORDER BY source_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                ("t-alex".to_string(), Some("alex".to_string())),
                ("t-gone".to_string(), Some("jordan".to_string())),
                ("t-open".to_string(), None),
            ]
        );
    }
}
