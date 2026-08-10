//! Load plan/exercises and attach them to [`FitnessCoachContext`].

use chrono::{Duration, Local, NaiveDate};
use chotu_common::{AppConfig, FitnessGoals};
use sqlx::SqlitePool;

use crate::coaching::FitnessCoachContext;
use crate::fitness_plan::{
    count_strength_sessions, current_week_start_str, load_weekly_plan, parse_plan_json,
    session_for_date, sum_cardio_minutes, week_start_monday,
};
use crate::sync::{exercise_entries_for_range, exercises_for_day};

/// Enrich a day/trend coach context with fitness goals, today's plan, and exercises.
pub async fn enrich_coach_context(
    pool: &SqlitePool,
    config: &AppConfig,
    member_id: &str,
    ctx: FitnessCoachContext,
    // When set, load exercises for this civil day; otherwise skip day exercises.
    exercise_date: Option<&str>,
) -> FitnessCoachContext {
    let member = config
        .family
        .members
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(member_id));
    let fitness = member
        .and_then(|m| m.fitness_goals.as_ref())
        .filter(|g| !g.is_empty());

    let today = Local::now().date_naive();
    let days_until = fitness.and_then(|g| g.days_until_target(today));

    let week_start = current_week_start_str();
    let planned = if let Ok(Some(stored)) = load_weekly_plan(pool, member_id, &week_start).await {
        parse_plan_json(&stored.plan_json)
            .ok()
            .and_then(|plan| session_for_date(&week_start, &plan, today).cloned())
    } else {
        None
    };

    let exercises = if let Some(date) = exercise_date {
        exercises_for_day(pool, member_id, date)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let week_start_date = week_start_monday(today);
    let week_end = (week_start_date + Duration::days(6))
        .format("%Y-%m-%d")
        .to_string();
    let week_start_s = week_start_date.format("%Y-%m-%d").to_string();
    let week_ex = exercise_entries_for_range(pool, member_id, &week_start_s, &week_end)
        .await
        .unwrap_or_default();

    let strength_done =
        count_strength_sessions(week_ex.iter().map(|e| e.activity_label()));
    let cardio_done = sum_cardio_minutes(
        week_ex
            .iter()
            .map(|e| (e.activity_label(), e.duration_mins())),
    );

    let targets = fitness.and_then(|g| g.weekly_targets.as_ref());
    let strength_target = targets.and_then(|t| t.strength_sessions);
    let cardio_target = targets.and_then(|t| t.cardio_minutes);

    ctx.with_fitness(
        fitness,
        days_until,
        planned.as_ref(),
        exercises,
        Some(strength_done),
        strength_target,
        Some(cardio_done),
        cardio_target,
    )
}

/// Outcome + today's session lines for briefs (indented under a member bullet).
pub fn fitness_brief_lines(
    fitness: &FitnessGoals,
    as_of: NaiveDate,
    planned_line: Option<&str>,
    yesterday_exercises: &[String],
) -> String {
    let mut out = String::new();
    if let Some(intent) = fitness
        .intent
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        out.push_str(&format!("  - Outcome: {}\n", intent));
    }
    if let Some(days) = fitness.days_until_target(as_of) {
        if days > 0 {
            out.push_str(&format!(
                "  - {} days to target ({})\n",
                days,
                fitness.target_date.as_deref().unwrap_or("")
            ));
        } else if days == 0 {
            out.push_str("  - Target day is today\n");
        } else {
            out.push_str(&format!("  - Target was {} days ago\n", -days));
        }
    }
    if let Some(p) = planned_line {
        out.push_str(&format!("  - Today: {}\n", p));
    } else {
        out.push_str("  - Today: _no plan yet — send `/plan`_\n");
    }
    if yesterday_exercises.is_empty() {
        out.push_str("  - Yesterday workouts: _none logged_\n");
    } else {
        out.push_str("  - Yesterday workouts:\n");
        for e in yesterday_exercises.iter().take(5) {
            out.push_str(&format!("    • {}\n", e));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chotu_common::FitnessGoals;

    #[test]
    fn brief_lines_include_countdown_and_session() {
        let fitness = FitnessGoals {
            intent: Some("beach body".into()),
            target_date: Some("2027-06-01".into()),
            focus: None,
            sessions_per_week: None,
            session_minutes: None,
            equipment: None,
            constraints: vec![],
            weekly_targets: None,
        };
        let as_of = NaiveDate::from_ymd_opt(2026, 8, 9).unwrap();
        let md = fitness_brief_lines(
            &fitness,
            as_of,
            Some("Tuesday (strength): lower body"),
            &["Weights 40m".into()],
        );
        assert!(md.contains("beach body"));
        assert!(md.contains("296 days"));
        assert!(md.contains("Tuesday (strength): lower body"));
        assert!(md.contains("Weights 40m"));
    }
}
