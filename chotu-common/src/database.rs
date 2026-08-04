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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
}
