use anyhow::Result;
use sqlx::SqlitePool;

mod sync;
mod trends;

pub use sync::{
    credentials_configured, delete_google_nutrition_logs, external_nutrition_base,
    google_data_point_ids_for_day, google_health_client_for_member, google_health_client_from_env,
    member_health_credentials_configured, push_food_log_to_google, push_pending_food_logs,
    rebuild_summary_from_food_log, sum_food_log_for_day, sum_unsynced_food_log_for_day,
    sync_configured_members_today, sync_member_for_date, sync_primary_today,
    write_summary_nutrition, DayNutritionTotals, HealthSyncReport,
};
pub use trends::build_nutrition_trend_reports;

/// Main entry point for the Health Coach Agent.
/// Owns the scheduled Google Health sync loop (8:45 PM local) so the coordinator
/// Telegram process does not need to duplicate that work.
pub async fn run(pool: SqlitePool, config: chotu_common::AppConfig) -> Result<()> {
    println!("Health Coach Agent starting (Google Health sync owner)...");

    if !credentials_configured() {
        println!(
            "Health Coach: Google Health credentials not configured \
             (FITBIT_CLIENT_ID / FITBIT_CLIENT_SECRET / HEALTH_REFRESH_TOKEN_<MEMBER> \
             or FITBIT_REFRESH_TOKEN). \
             Scheduled sync disabled; /sync will still report the missing credentials."
        );
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
        }
    }

    let gemini = match std::env::var("GEMINI_API_KEY") {
        Ok(k) => Some(chotu_common::GeminiClient::new(k)),
        Err(_) => {
            eprintln!(
                "Health Coach: GEMINI_API_KEY missing; scheduled sync will still run, \
                 but omega-3 / triglyceride estimates will be zero."
            );
            None
        }
    };

    let linked: Vec<String> = config
        .family
        .members
        .iter()
        .filter(|m| member_health_credentials_configured(&m.id, &config))
        .map(|m| m.id.clone())
        .collect();

    println!(
        "Health Coach: Proactive Google Health nutrition sync enabled for: {}",
        if linked.is_empty() {
            "(none — tokens missing)".to_string()
        } else {
            linked.join(", ")
        }
    );

    let mut last_sync_date = String::new();
    loop {
        use chrono::Timelike;
        let now = chrono::Local::now();
        let date_str = now.format("%Y-%m-%d").to_string();

        // Trigger at 8:45 PM (20:45) every evening before reflection prompts are generated
        if now.hour() == 20 && now.minute() == 45 && date_str != last_sync_date {
            println!(
                "Health Coach: Scheduled time (8:45 PM) reached. Syncing nutrition from Google Health..."
            );
            match sync_configured_members_today(&pool, gemini.as_ref(), &config).await {
                Ok(reports) => {
                    for report in &reports {
                        println!(
                            "Health Coach: Sync complete for {} — {} kcal, {} steps",
                            report.member_id, report.calories, report.steps
                        );
                        notify_telegram(&report.telegram_markdown()).await;
                    }
                    last_sync_date = date_str;
                }
                Err(e) => {
                    eprintln!("Health Coach: scheduled Google Health sync failed: {:?}", e);
                }
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
    }
}

async fn notify_telegram(message: &str) {
    if let (Ok(token), Ok(chat_id_str)) = (
        std::env::var("TELEGRAM_BOT_TOKEN").or_else(|_| std::env::var("TELOXIDE_TOKEN")),
        std::env::var("TELEGRAM_CHAT_ID"),
    ) {
        if let Ok(chat_id_num) = chat_id_str.parse::<i64>() {
            let bot = teloxide::Bot::new(token);
            use teloxide::prelude::*;
            #[allow(deprecated)]
            if let Err(e) = bot
                .send_message(teloxide::types::ChatId(chat_id_num), message)
                .parse_mode(teloxide::types::ParseMode::Markdown)
                .await
            {
                eprintln!("Health Coach: failed to push sync notification: {:?}", e);
            }
        }
    }
}
