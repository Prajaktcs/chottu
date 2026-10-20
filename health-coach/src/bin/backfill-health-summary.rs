use anyhow::{bail, Context, Result};
use chrono::{Duration, NaiveDate};
use sqlx::SqlitePool;

const USAGE: &str = "backfill-health-summary [--from YYYY-MM-DD] [--to YYYY-MM-DD] \
    [--member ID] [--apply]\n\
    Rebuilds past daily nutrition summaries from Google Health entries. \
    Dry run by default: prints stored vs recomputed totals without writing. \
    --from defaults to the earliest logged day, --to to yesterday, --member to \
    every member with Google Health credentials. Days with unsynced local meals \
    (possible duplicates) or fewer Google calories than stored are skipped and \
    listed for manual review. Pending meals are never pushed. \
    Back up the database first. See docs/commands/health.md.";

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
    let pool = chotu_common::init_db(&db_path).await?;
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
    let today = config.now_in_tz().date_naive();
    // Today is owned by the regular sync, which also pushes pending meals.
    let to = to
        .unwrap_or(today - Duration::days(1))
        .min(today - Duration::days(1));

    println!(
        "{} for {} ({db_path})",
        if apply {
            "Applying backfill"
        } else {
            "Dry run (no writes)"
        },
        members.join(", ")
    );
    let mut skipped = Vec::new();
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
        println!("  date        kcal stored→google   sat fat g stored→new");
        let mut date = from;
        while date <= to {
            let day = date.format("%Y-%m-%d").to_string();
            let mut attempt = 1;
            let skip = loop {
                let run = Day {
                    pool: &pool,
                    config: &config,
                    client: &client,
                    gemini: gemini.as_ref(),
                    member_id,
                    day: &day,
                };
                match run.backfill(apply).await {
                    Ok(skip) => break skip,
                    // Google Health returns sporadic 503s over long ranges.
                    Err(e) if attempt < 4 => {
                        eprintln!("  {day}: attempt {attempt} failed, retrying: {e:#}");
                        tokio::time::sleep(std::time::Duration::from_secs(5 * attempt)).await;
                        attempt += 1;
                    }
                    Err(e) => {
                        return Err(e.context(format!("Backfill failed at {member_id} {day}")))
                    }
                }
            };
            if skip {
                skipped.push(format!("{member_id} {day}"));
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
    if !apply {
        println!("\nNothing written. Re-run with --apply to rewrite these days.");
    }
    pool.close().await;
    Ok(())
}

struct Day<'a> {
    pool: &'a SqlitePool,
    config: &'a chotu_common::AppConfig,
    client: &'a chotu_common::google_health::GoogleHealthClient,
    gemini: Option<&'a chotu_common::GeminiClient>,
    member_id: &'a str,
    day: &'a str,
}

impl Day<'_> {
    /// Prints stored vs recomputed totals and, with `apply`, rewrites the day.
    /// Returns true when the day is left unchanged for manual review.
    async fn backfill(&self, apply: bool) -> Result<bool> {
        let Day {
            pool,
            config,
            client,
            gemini,
            member_id,
            day,
        } = *self;
        let (stored_kcal, stored_sat) = stored_summary(pool, member_id, day).await?;
        let google = client.fetch_nutrition_summary(day).await?;
        let local =
            health_coach::sum_unsynced_food_log_for_day(pool, member_id, day, config.resolved_tz())
                .await?;
        let new_kcal = google.calories.round() as i64 + local.calories;
        let new_sat = google.saturated_fat + local.saturated_fat_g;
        // Old unsynced meals may already be in Google (a push whose id was never
        // recorded, or the same meal logged in another app), so adding them would
        // double count. Fewer Google calories than stored means entries went missing.
        let skip = if local.calories > 0 {
            Some("has unsynced local meals; check Google for duplicates")
        } else if new_kcal + 1 < stored_kcal {
            Some("Google has fewer calories than stored")
        } else {
            None
        };
        let note = skip.map(|why| format!("  SKIP: {why}")).unwrap_or_default();
        if stored_kcal != 0 || new_kcal != 0 {
            println!("  {day}  nutrition totals redacted{note}");
        }
        if apply && skip.is_none() && new_kcal != 0 {
            health_coach::resync_member_for_past_date(pool, gemini, config, member_id, day).await?;
        }
        Ok(skip.is_some())
    }
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

async fn stored_summary(pool: &SqlitePool, member_id: &str, day: &str) -> Result<(i64, f64)> {
    let row: Option<(i64, f64)> = sqlx::query_as(
        "SELECT COALESCE(total_calories_ingested, 0), COALESCE(saturated_fat_g, 0.0) \
         FROM health_family_summary WHERE family_member_id = ? AND date = ?",
    )
    .bind(member_id)
    .bind(day)
    .fetch_optional(pool)
    .await?;
    Ok(row.unwrap_or((0, 0.0)))
}
