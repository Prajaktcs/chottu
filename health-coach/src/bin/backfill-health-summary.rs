use anyhow::{bail, Context, Result};
use chrono::{Duration, NaiveDate};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;

const USAGE: &str = "backfill-health-summary [--from YYYY-MM-DD] [--to YYYY-MM-DD] \
    [--member ID] [--apply]\n\
    Rebuilds past daily summaries from Google Health entries. \
    Dry run by default: opens the database read-only and prints stored vs \
    recomputed totals. --from defaults to the earliest logged day, --to to \
    yesterday, --member to every member with Google Health credentials. Days \
    with unsynced local meals (possible duplicates) or fewer Google calories \
    than stored are skipped and listed for manual review. Nothing is pushed \
    to Google. Run the upgraded app once first (migrations) and back up the \
    database. See docs/commands/health.md.";

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let mut from = None;
    let mut to = None;
    let mut member = None;
    let mut apply = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--apply" => apply = true,
            "--from" => from = Some(parse_date(args.next().context("--from needs a date")?)?),
            "--to" => to = Some(parse_date(args.next().context("--to needs a date")?)?),
            "--member" => member = Some(args.next().context("--member needs an ID")?),
            _ => bail!("Unknown argument {arg}\n{USAGE}"),
        }
    }

    let db_path = std::env::var("DATABASE_PATH").unwrap_or_else(|_| "chotu.db".to_string());
    let config_path =
        std::env::var("CHOTU_CONFIG_PATH").unwrap_or_else(|_| "config.yaml".to_string());
    let config = chotu_common::load_config(&config_path)
        .map_err(anyhow::Error::msg)
        .context("Failed to load configuration")?;
    // Not init_db: that creates, migrates and backfills. A dry run must not write.
    let options = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(false)
        .read_only(!apply)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(30));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .with_context(|| format!("Cannot open existing database {db_path}"))?;
    let gemini = std::env::var("GEMINI_API_KEY")
        .ok()
        .map(chotu_common::GeminiClient::new);

    let members: Vec<String> = match member {
        Some(id) => vec![id],
        None => config
            .family
            .members
            .iter()
            .filter(|m| health_coach::member_health_credentials_configured(&m.id, &config))
            .map(|m| m.id.clone())
            .collect(),
    };
    if members.is_empty() {
        bail!("No members with Google Health credentials");
    }
    let yesterday = config.now_in_tz().date_naive() - Duration::days(1);
    // Today is owned by the regular sync, which also pushes pending meals.
    let to = to.unwrap_or(yesterday).min(yesterday);

    println!(
        "{} for {} ({db_path})",
        if apply {
            "Applying backfill"
        } else {
            "Dry run (read-only)"
        },
        members.join(", ")
    );
    let mut skipped = Vec::new();
    let mut written = 0;
    for member_id in &members {
        let from = match from {
            Some(d) => d,
            None => match earliest_logged_day(&pool, member_id).await? {
                Some(d) => d,
                None => continue,
            },
        };
        let client = health_coach::google_health_client_for_member(member_id, &config)?;
        println!("\n{member_id}: {from} → {to}");
        println!("  date        kcal stored→google   sat fat g stored→new   steps stored→new");
        let mut date = from;
        while date <= to {
            let day = date.format("%Y-%m-%d").to_string();
            let mut attempt = 1;
            let past = loop {
                match health_coach::fetch_past_day(&pool, &client, &config, member_id, &day).await {
                    Ok(past) => break past,
                    // Google Health returns sporadic 503s over long ranges.
                    Err(_) if attempt < 4 => {
                        eprintln!("  {day}: fetch attempt {attempt} failed, retrying");
                        tokio::time::sleep(std::time::Duration::from_secs(5 * attempt)).await;
                        attempt += 1;
                    }
                    Err(e) => {
                        return Err(e.context(format!("Backfill failed at {member_id} {day}")))
                    }
                }
            };
            let note = past
                .skip_reason
                .map(|why| format!("  SKIP: {why}"))
                .unwrap_or_default();
            if past.has_data || past.skip_reason.is_some() {
                println!(
                    "  {day}  {:>6}→{:<6}       {:>6.1}→{:<6.1}      {:>6}→{:<6}{note}",
                    past.stored_calories,
                    past.calories,
                    past.stored_saturated_fat_g,
                    past.saturated_fat_g,
                    past.stored_steps,
                    past.steps
                );
            }
            if past.skip_reason.is_some() {
                skipped.push(format!("{member_id} {day}"));
            } else if apply && past.has_data {
                health_coach::write_past_day(
                    &pool,
                    gemini.as_ref(),
                    &config,
                    member_id,
                    &day,
                    past,
                )
                .await
                .with_context(|| format!("Backfill failed at {member_id} {day}"))?;
                written += 1;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            date += Duration::days(1);
        }
    }
    if !skipped.is_empty() {
        println!(
            "\nLeft unchanged ({}): {}",
            skipped.len(),
            skipped.join(", ")
        );
    }
    if apply {
        println!("\nRewrote {written} day(s).");
    } else {
        println!("\nNothing written. Re-run with --apply to rewrite these days.");
    }
    pool.close().await;
    Ok(())
}

fn parse_date(value: String) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(&value, "%Y-%m-%d").with_context(|| format!("Bad date {value}"))
}

async fn earliest_logged_day(pool: &SqlitePool, member_id: &str) -> Result<Option<NaiveDate>> {
    let earliest: Option<String> = sqlx::query_scalar(
        "SELECT MIN(d) FROM (\
            SELECT MIN(date) AS d FROM health_family_summary WHERE family_member_id = ? \
            UNION ALL \
            SELECT MIN(date(timestamp)) FROM food_log WHERE family_member_id = ?)",
    )
    .bind(member_id)
    .bind(member_id)
    .fetch_one(pool)
    .await?;
    earliest.map(parse_date).transpose()
}
