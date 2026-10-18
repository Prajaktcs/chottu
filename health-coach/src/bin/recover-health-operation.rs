use anyhow::{bail, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "recover-health-operation --apply --database PATH --backup NEW_PATH \
    --member ID --meal ID --response ORIGINAL_COMPLETED_RESPONSE.json\n\
    Stop all database writers first. Use only a trusted original Google Health \
    completed create response; this tool cannot authenticate a JSON file. \
    Run the upgraded application once to apply migrations before recovery. \
    No network requests are made. See docs/commands/health.md.";

#[tokio::main]
async fn main() -> Result<()> {
    let mut database = None;
    let mut backup = None;
    let mut member = None;
    let mut meal = None;
    let mut response = None;
    let mut apply = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--apply" => apply = true,
            "--database" => {
                database = Some(PathBuf::from(
                    args.next().context("--database needs a path")?,
                ))
            }
            "--backup" => {
                backup = Some(PathBuf::from(
                    args.next().context("--backup needs a new path")?,
                ))
            }
            "--member" => member = Some(args.next().context("--member needs an ID")?),
            "--meal" => meal = Some(args.next().context("--meal needs an ID")?),
            "--response" => {
                response = Some(PathBuf::from(
                    args.next().context("--response needs a path")?,
                ))
            }
            _ => bail!("Unknown argument {arg}\n{USAGE}"),
        }
    }
    if !apply {
        bail!("Recovery requires --apply\n{USAGE}");
    }
    let database = database.context("Missing --database")?;
    let backup = backup.context("Missing --backup")?;
    let member = member.context("Missing --member")?;
    let meal = meal.context("Missing --meal")?;
    let response = response.context("Missing --response")?;
    if !database.is_file() {
        bail!("Database must already exist");
    }
    if backup.exists() {
        bail!("Refusing to overwrite backup {}", backup.display());
    }
    let completed_response: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&response).context("Cannot read original completed Operation response")?,
    )
    .context("Invalid Operation response JSON; no recovery was applied")?;
    chotu_common::google_health::completed_nutrition_operation_mapping(&completed_response)?;

    let options = SqliteConnectOptions::new()
        .filename(&database)
        .create_if_missing(false)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(30));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    // This repair must not run unrelated application backfills or migrations.
    sqlx::query("SELECT operation_name FROM food_log_operation_recoveries LIMIT 0")
        .execute(&pool)
        .await
        .context("Recovery schema missing; run the upgraded application once, then stop it")?;
    // VACUUM INTO includes committed WAL data and refuses an existing destination.
    sqlx::query("VACUUM INTO ?")
        .bind(backup.to_str().context("Backup path must be UTF-8")?)
        .execute(&pool)
        .await
        .context("Cannot create database backup; recovery not started")?;
    println!("Backup: {}", backup.display());
    let name =
        health_coach::recover_legacy_food_operation(&pool, &member, &meal, &completed_response)
            .await?;
    println!("Recorded {member}/{meal} -> {name}. No remote changes made. Restart Chotu and run /sync to resume authorized cleanup.");
    pool.close().await;
    Ok(())
}
