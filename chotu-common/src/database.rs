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
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='tasks'"
    )
    .fetch_one(&pool)
    .await
    .unwrap_or((0,));

    let mut migrated_tasks = false;

    if table_exists.0 > 0 {
        let has_created_at: (i32,) = sqlx::query_as(
            "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name='created_at'"
        )
        .fetch_one(&pool)
        .await
        .unwrap_or((0,));

        if has_created_at.0 == 0 {
            println!("Database: Pre-migration - Renaming tasks to tasks_old and preparing new tasks table...");
            let mut tx = pool.begin().await?;

            // 1. Rename old table
            sqlx::query("ALTER TABLE tasks RENAME TO tasks_old;")
                .execute(&mut *tx)
                .await?;

            // 2. Create new table (base schema matching original 20260620000000 checksum)
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
                );"
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
            "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name='message_id'"
        )
        .fetch_one(&pool)
        .await
        .unwrap_or((0,));

        let migration_applied: (i32,) = sqlx::query_as(
            "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 20260620000001"
        )
        .fetch_one(&pool)
        .await
        .unwrap_or((0,));

        if has_message_id.0 > 0 && migration_applied.0 == 0 {
            println!("Database: Clean up duplicate message_id column for migration compatibility...");
            if let Err(_) = sqlx::query("ALTER TABLE tasks DROP COLUMN message_id;").execute(&pool).await {
                let mut tx = pool.begin().await?;
                let _ = sqlx::query("DROP INDEX IF EXISTS idx_tasks_message_id;").execute(&mut *tx).await;
                let _ = sqlx::query("ALTER TABLE tasks RENAME TO tasks_temp;").execute(&mut *tx).await;
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
                    );"
                ).execute(&mut *tx).await;
                let _ = sqlx::query(
                    "INSERT INTO tasks (id, created_at, updated_at, title, description, assigned_to, due_date, duration_minutes, priority, status, calendar_event_id, source) \
                     SELECT id, created_at, updated_at, title, description, assigned_to, due_date, duration_minutes, priority, status, calendar_event_id, source \
                     FROM tasks_temp;"
                ).execute(&mut *tx).await;
                let _ = sqlx::query("DROP TABLE tasks_temp;").execute(&mut *tx).await;
                tx.commit().await?;
            }
        }
    }

    // Run embedded migrations (looks in the crate's migrations folder by default)
    // This will run any pending migrations, including 20260620000001 which adds message_id to tasks.
    sqlx::migrate!()
        .run(&pool)
        .await
        .context("Failed to run database migrations")?;

    // ─── Post-Migration Data Migration ──────────────────────────────────────────────
    // If we renamed tasks to tasks_old, we now copy all the data over and drop the old table.
    // Both tables are now guaranteed to have their final schemas (including message_id).
    if migrated_tasks {
        let has_tasks_old: (i32,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='tasks_old'"
        )
        .fetch_one(&pool)
        .await
        .unwrap_or((0,));

        if has_tasks_old.0 > 0 {
            println!("Database: Post-migration - Migrating tasks data and clean up tasks_old...");
            let mut tx = pool.begin().await?;

            // Check if tasks_old has message_id column
            let has_message_id: (i32,) = sqlx::query_as(
                "SELECT COUNT(*) FROM pragma_table_info('tasks_old') WHERE name='message_id'"
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
                     FROM tasks_old;"
                )
                .execute(&mut *tx)
                .await?;
            } else {
                sqlx::query(
                    "INSERT INTO tasks (id, created_at, updated_at, title, status, source) \
                     SELECT id, timestamp, timestamp, task_description, \
                            CASE WHEN status = 'completed' OR status = 'done' THEN 'done' ELSE 'open' END, \
                            'manual' \
                     FROM tasks_old;"
                )
                .execute(&mut *tx)
                .await?;
            }

            // Drop old table
            sqlx::query("DROP TABLE tasks_old;")
                .execute(&mut *tx)
                .await?;

            tx.commit().await?;
            println!("Database: Post-migration tasks migration completed successfully!");
        }
    }

    Ok(pool)
}
