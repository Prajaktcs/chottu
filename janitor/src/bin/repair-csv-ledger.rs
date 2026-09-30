use anyhow::{bail, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::path::PathBuf;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let mut database = None;
    let mut archive = None;
    let mut backup = None;
    let mut currency = None;
    let mut apply = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--apply" => apply = true,
            "--database" => {
                database = Some(PathBuf::from(
                    args.next().context("--database needs a path")?,
                ))
            }
            "--archive" => {
                archive = Some(PathBuf::from(
                    args.next().context("--archive needs a directory")?,
                ))
            }
            "--backup" => {
                backup = Some(PathBuf::from(
                    args.next().context("--backup needs a new path")?,
                ))
            }
            "--currency" => {
                currency = Some(args.next().context("--currency needs an ISO currency")?)
            }
            _ => bail!("Unknown argument {arg}"),
        }
    }
    if !apply {
        bail!("Repair requires --apply --database PATH --archive DIR --backup NEW_PATH --currency ISO. Rehearse on a copy first.");
    }
    let database = database.context("Missing --database")?;
    let archive = archive.context("Missing --archive")?;
    let backup = backup.context("Missing --backup")?;
    let currency = currency.context("Missing --currency")?;
    if !database.is_file() || !archive.is_dir() {
        bail!("Database must exist and archive must be a directory");
    }
    if backup.exists() {
        bail!("Refusing to overwrite backup {}", backup.display());
    }
    let options = SqliteConnectOptions::new()
        .filename(&database)
        .create_if_missing(false)
        .busy_timeout(Duration::from_secs(30));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    // VACUUM INTO includes committed WAL contents and produces a consistent, standalone snapshot.
    // Do not run application init_db: repairing CSVs must not mutate other tables or run backfills.
    sqlx::query("VACUUM INTO ?")
        .bind(backup.to_str().context("Backup path must be UTF-8")?)
        .execute(&pool)
        .await
        .context("Cannot create database backup; repair not started")?;
    println!("Backup: {}", backup.display());
    let stats = janitor::csv_import::rebuild_archived_csvs(&pool, &archive, &currency).await?;
    println!("Files: {}; source transactions: {}; blank rows: {}; legacy CSV rows removed: {}; corrected transactions inserted: {}; overlapping source rows matched: {}; metadata upgrades: {}",
        stats.files, stats.source_rows, stats.blank_rows, stats.legacy_removed,
        stats.inserted, stats.matched, stats.updated);
    pool.close().await;
    Ok(())
}
