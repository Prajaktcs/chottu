use anyhow::Result;
use sqlx::SqlitePool;

mod coach_enrich;
mod coaching;
mod fitness_plan;
mod sync;
mod trends;

pub use coach_enrich::{enrich_coach_context, fitness_brief_lines};
pub use coaching::{
    append_coach_tip, generate_fitness_coach_tip, generate_nutrition_coach_tip,
    FitnessCoachContext, NutritionCoachContext,
};
pub use fitness_plan::{
    count_strengthish_sessions, current_week_start_str, generate_and_store_weekly_plan,
    load_weekly_plan, parse_plan_json, render_plan_markdown, session_for_date,
    session_for_date_from_stored, week_start_monday, weekday_name, PlanDay, PlanDayKind,
    StoredWeeklyPlan, WeeklyFitnessPlan,
};
pub use sync::{
    credentials_configured, delete_google_nutrition_logs, exercises_for_day, exercises_for_range,
    external_nutrition_base, google_data_point_ids_for_day, google_health_client_for_member,
    google_health_client_from_env, member_health_credentials_configured, push_food_log_to_google,
    push_pending_food_logs, rebuild_summary_from_food_log, replace_exercise_log_for_day,
    sum_food_log_for_day, sum_unsynced_food_log_for_day, sync_configured_members_today,
    sync_member_for_date, sync_primary_today, write_summary_nutrition, DayNutritionTotals,
    HealthSyncReport,
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
                        notify_member_telegram(&report.telegram_markdown(), &config, &report.member_id)
                            .await;
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

/// Deliver a member's health sync only to their linked DM (never other adults' chats).
async fn notify_member_telegram(
    message: &str,
    config: &chotu_common::AppConfig,
    member_id: &str,
) {
    let Ok(token) =
        std::env::var("TELEGRAM_BOT_TOKEN").or_else(|_| std::env::var("TELOXIDE_TOKEN"))
    else {
        return;
    };
    let targets: Vec<i64> =
        if let Some(cid) = chotu_common::telegram_chat_for_member(config, member_id) {
            vec![cid]
        } else if !chotu_common::has_any_telegram_link(config) {
            // Pre-/link single-user setups: optional TELEGRAM_CHAT_ID fallback.
            chotu_common::telegram_delivery_targets(config)
        } else {
            // Other members are linked; do not broadcast this person's metrics into their DMs.
            Vec::new()
        };
    if targets.is_empty() {
        return;
    }
    let bot = teloxide::Bot::new(token);
    use teloxide::prelude::*;
    for cid in targets {
        #[allow(deprecated)]
        if let Err(e) = bot
            .send_message(teloxide::types::ChatId(cid), message)
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .await
        {
            eprintln!(
                "Health Coach: failed to push sync notification to {}: {:?}",
                cid, e
            );
        }
    }
}
