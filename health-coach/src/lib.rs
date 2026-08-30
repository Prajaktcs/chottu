use anyhow::Result;
use sqlx::SqlitePool;

mod coach_enrich;
mod coaching;
mod fitness_plan;
mod sync;
mod trends;

pub use coach_enrich::{
    enrich_coach_context, fitness_brief_lines, format_plan_progress_line, plan_week_progress_line,
    CoachEnrichOpts,
};
pub use coaching::{
    append_coach_tip, generate_fitness_coach_tip, generate_nutrition_coach_tip,
    FitnessCoachContext, NutritionCoachContext,
};
pub use fitness_plan::{
    activity_matches_plan_kind, classify_activity_type, count_strength_sessions,
    count_strengthish_sessions, current_week_start_str, generate_and_store_weekly_plan,
    load_weekly_plan, parse_plan_json, plan_cardio_minutes_on_cardio_days,
    plan_session_adherence, render_plan_markdown, session_for_date, session_for_date_from_stored,
    sum_cardio_minutes, week_start_monday, weekday_name, ActivityKind, PlanDay, PlanDayKind,
    StoredWeeklyPlan, WeeklyFitnessPlan,
};
pub use sync::{
    credentials_configured, delete_google_nutrition_logs, exercise_entries_for_day,
    exercise_entries_for_range, exercises_for_day, exercises_for_range, external_nutrition_base,
    google_data_point_ids_for_day, google_health_client_for_member, google_health_client_from_env,
    member_health_credentials_configured, push_food_log_to_google, push_pending_food_logs,
    rebuild_summary_from_food_log, replace_exercise_log_for_day, sum_food_log_for_day,
    sum_unsynced_food_log_for_day, sync_configured_members_today, sync_member_for_date,
    sync_primary_today, write_summary_nutrition, DayNutritionTotals, ExerciseLogEntry,
    HealthSyncReport,
};
pub use trends::build_nutrition_trend_reports;

/// Default daily step target when `nutrition_goals.steps` is unset.
const DEFAULT_STEPS_GOAL: i32 = 10_000;

/// Main entry point for the Health Coach Agent.
/// Owns scheduled Google Health sync from `config.yaml` `schedules`
/// (`health_evening_sync`, `health_late_steps`) in the agent IANA `timezone`.
/// Blank slots are not scheduled.
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

    let tz_name = config.resolved_timezone_name();
    let evening = config.schedule_clock(chotu_common::AgentSchedules::health_evening_sync);
    let late = config.schedule_clock(chotu_common::AgentSchedules::health_late_steps);

    println!(
        "Health Coach: Proactive Google Health sync enabled for: {}",
        if linked.is_empty() {
            "(none — tokens missing)".to_string()
        } else {
            linked.join(", ")
        }
    );
    let fmt_slot = |clock: Option<chotu_common::ClockTime>| match clock {
        Some(t) => format!("{:02}:{:02} {}", t.hour, t.minute, tz_name),
        None => "off".to_string(),
    };
    println!(
        "Health Coach: Evening sync {}; late steps sync {} + step-goal nudge.",
        fmt_slot(evening),
        fmt_slot(late)
    );

    let mut last_evening_sync_date = String::new();
    let mut last_late_sync_date = String::new();
    loop {
        let now = config.now_in_tz();
        let date_str = now.format("%Y-%m-%d").to_string();

        if let Some(clock) = evening {
            if clock.matches(now) && date_str != last_evening_sync_date {
                println!(
                    "Health Coach: Evening sync ({:02}:{:02} {}). Pulling Google Health...",
                    clock.hour, clock.minute, tz_name
                );
                match sync_configured_members_today(&pool, gemini.as_ref(), &config).await {
                    Ok(reports) => {
                        for report in &reports {
                            println!(
                                "Health Coach: Evening sync complete for {} — {} kcal, {} steps",
                                report.member_id, report.calories, report.steps
                            );
                            notify_member_signal(
                                &report.telegram_markdown(),
                                &config,
                                &report.member_id,
                            )
                            .await;
                        }
                        last_evening_sync_date = date_str.clone();
                    }
                    Err(e) => {
                        eprintln!("Health Coach: evening Google Health sync failed: {:?}", e);
                    }
                }
            }
        }

        if let Some(clock) = late {
            if clock.matches(now) && date_str != last_late_sync_date {
                println!(
                    "Health Coach: Late steps sync ({:02}:{:02} {}). Pulling Google Health...",
                    clock.hour, clock.minute, tz_name
                );
                match sync_configured_members_today(&pool, gemini.as_ref(), &config).await {
                    Ok(reports) => {
                        for report in &reports {
                            let goal = steps_goal_for_member(&config, &report.member_id);
                            println!(
                                "Health Coach: Late sync complete for {} — {}/{} steps",
                                report.member_id, report.steps, goal
                            );
                            notify_member_signal(
                                &steps_nudge_markdown(report, goal),
                                &config,
                                &report.member_id,
                            )
                            .await;
                        }
                        last_late_sync_date = date_str;
                    }
                    Err(e) => {
                        eprintln!("Health Coach: late Google Health sync failed: {:?}", e);
                    }
                }
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
    }
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
async fn notify_member_signal(
    message: &str,
    config: &chotu_common::AppConfig,
    member_id: &str,
) {
    let socket = match std::env::var("SIGNAL_CLI_SOCKET") {
        Ok(path) if !path.trim().is_empty() => path,
        _ => return,
    };
    let targets = if let Some(aci) = chotu_common::signal_aci_for_member(config, member_id) {
        vec![chotu_common::SignalRecipient::Direct { aci }]
    } else if !chotu_common::has_any_signal_link(config) {
        chotu_common::signal_delivery_targets(config)
    } else {
        Vec::new()
    };
    if targets.is_empty() {
        return;
    }
    let client = match chotu_common::SignalClient::connect(&socket).await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("Health Coach: SIGNAL_CLI_SOCKET is unreachable ({socket}): {error:?}");
            return;
        }
    };
    for recipient in targets {
        if let Err(error) = client.send_text(&recipient, message).await {
            eprintln!(
                "Health Coach: failed to push sync notification to {recipient}: {error:?}"
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
}
