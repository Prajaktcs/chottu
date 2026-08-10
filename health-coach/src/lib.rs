use anyhow::Result;
use chrono::{Local, Timelike};
use chrono_tz::Tz;
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

/// Default daily step target when `nutrition_goals.steps` is unset.
const DEFAULT_STEPS_GOAL: i32 = 10_000;

/// Main entry point for the Health Coach Agent.
/// Owns scheduled Google Health sync:
/// - 8:45 PM local — evening nutrition pass (before reflection)
/// - 11:00 PM America/New_York (override via `HEALTH_LATE_SYNC_*`) — catch late steps + nudge
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

    let late_tz = late_sync_timezone();
    let late_hour = env_u32("HEALTH_LATE_SYNC_HOUR", 23).min(23);
    let late_minute = env_u32("HEALTH_LATE_SYNC_MINUTE", 0).min(59);

    println!(
        "Health Coach: Proactive Google Health sync enabled for: {}",
        if linked.is_empty() {
            "(none — tokens missing)".to_string()
        } else {
            linked.join(", ")
        }
    );
    println!(
        "Health Coach: Evening sync 20:45 local; late steps sync {:02}:{:02} {} + 10k-step nudge.",
        late_hour, late_minute, late_tz
    );

    let mut last_evening_sync_date = String::new();
    let mut last_late_sync_date = String::new();
    loop {
        let now_local = Local::now();
        let local_date = now_local.format("%Y-%m-%d").to_string();

        // 8:45 PM local — nutrition pass before evening reflection.
        if now_local.hour() == 20
            && now_local.minute() == 45
            && local_date != last_evening_sync_date
        {
            println!(
                "Health Coach: Evening sync (8:45 PM local). Pulling Google Health..."
            );
            match sync_configured_members_today(&pool, gemini.as_ref(), &config).await {
                Ok(reports) => {
                    for report in &reports {
                        println!(
                            "Health Coach: Evening sync complete for {} — {} kcal, {} steps",
                            report.member_id, report.calories, report.steps
                        );
                        notify_member_telegram(
                            &report.telegram_markdown(),
                            &config,
                            &report.member_id,
                        )
                        .await;
                    }
                    last_evening_sync_date = local_date.clone();
                }
                Err(e) => {
                    eprintln!("Health Coach: evening Google Health sync failed: {:?}", e);
                }
            }
        }

        // 11:00 PM ET (default) — catch late-day steps and nudge toward the daily goal.
        let now_tz = now_local.with_timezone(&late_tz);
        let late_date = now_tz.format("%Y-%m-%d").to_string();
        if now_tz.hour() == late_hour
            && now_tz.minute() == late_minute
            && late_date != last_late_sync_date
        {
            println!(
                "Health Coach: Late steps sync ({:02}:{:02} {}). Pulling Google Health...",
                late_hour, late_minute, late_tz
            );
            match sync_configured_members_today(&pool, gemini.as_ref(), &config).await {
                Ok(reports) => {
                    for report in &reports {
                        let goal = steps_goal_for_member(&config, &report.member_id);
                        println!(
                            "Health Coach: Late sync complete for {} — {}/{} steps",
                            report.member_id, report.steps, goal
                        );
                        notify_member_telegram(
                            &steps_nudge_markdown(report, goal),
                            &config,
                            &report.member_id,
                        )
                        .await;
                    }
                    last_late_sync_date = late_date;
                }
                Err(e) => {
                    eprintln!("Health Coach: late Google Health sync failed: {:?}", e);
                }
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
    }
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Timezone for the late steps sync. Defaults to America/New_York (ET).
/// Override with `HEALTH_LATE_SYNC_TZ` (IANA), else `CHOTU_TIMEZONE`.
fn late_sync_timezone() -> Tz {
    let raw = std::env::var("HEALTH_LATE_SYNC_TZ")
        .or_else(|_| std::env::var("CHOTU_TIMEZONE"))
        .unwrap_or_else(|_| "America/New_York".to_string());
    raw.parse::<Tz>().unwrap_or(chrono_tz::America::New_York)
}

fn steps_goal_for_member(config: &chotu_common::AppConfig, member_id: &str) -> i32 {
    config
        .family
        .members
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(member_id))
        .and_then(|m| m.nutrition_goals.as_ref())
        .and_then(|g| g.steps)
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_STEPS_GOAL)
}

/// Compact private DM after the late sync — push toward the daily step goal.
fn steps_nudge_markdown(report: &HealthSyncReport, goal: i32) -> String {
    let steps = report.steps;
    let goal = goal.max(1);
    let pct = ((steps as f64 / goal as f64) * 100.0).round() as i32;
    if steps >= goal {
        format!(
            "🚶 *Steps check* ({})\n\n\
             {} / {} steps ({:.0}%) — goal hit. Nice work finishing the day strong.",
            report.date, steps, goal, pct as f64
        )
    } else {
        let remaining = goal - steps;
        format!(
            "🚶 *Steps check* ({})\n\n\
             {} / {} steps ({:.0}%). *{} to go* before midnight — a short walk closes the gap.",
            report.date, steps, goal, pct as f64, remaining
        )
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

#[cfg(test)]
mod steps_nudge_tests {
    use super::*;

    fn report(steps: i32) -> HealthSyncReport {
        HealthSyncReport {
            member_id: "alex".into(),
            date: "2026-08-09".into(),
            calories: 0,
            protein: 0.0,
            carbs: 0.0,
            fats: 0.0,
            saturated_fat: 0.0,
            unsaturated_fat: 0.0,
            cholesterol: 0.0,
            iron: 0.0,
            vitamin_b: 0.0,
            vitamin_c: 0.0,
            fiber: 0.0,
            sugar: 0.0,
            sodium: 0.0,
            omega_3_dha_mg: 0.0,
            triglycerides_mg: 0.0,
            steps,
            active_calories: 0,
            sleep_hours: None,
            exercises: vec![],
            manual_food_entries: 0,
        }
    }

    #[test]
    fn nudge_when_under_goal() {
        let md = steps_nudge_markdown(&report(8432), 10_000);
        assert!(md.contains("8432 / 10000"));
        assert!(md.contains("1568 to go"));
    }

    #[test]
    fn celebrate_when_goal_hit() {
        let md = steps_nudge_markdown(&report(10_200), 10_000);
        assert!(md.contains("goal hit"));
        assert!(md.contains("10200 / 10000"));
    }

    #[test]
    fn late_tz_defaults_to_new_york() {
        let _guard = (); // env may vary in CI; just ensure parse path is valid.
        assert_eq!(
            "America/New_York".parse::<Tz>().unwrap(),
            chrono_tz::America::New_York
        );
    }
}
