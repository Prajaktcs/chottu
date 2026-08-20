//! Load plan/exercises and attach them to [`FitnessCoachContext`].

use chrono::{Duration, Local, NaiveDate};
use chotu_common::{AppConfig, FitnessGoals};
use sqlx::SqlitePool;

use crate::coaching::FitnessCoachContext;
use crate::fitness_plan::{
    count_strength_sessions, current_week_start_str, load_weekly_plan, parse_plan_json,
    plan_cardio_minutes_on_cardio_days, plan_session_adherence, session_for_date,
    sum_cardio_minutes, week_start_monday, WeeklyFitnessPlan,
};
use crate::sync::{exercise_entries_for_range, exercises_for_day};

/// Options for [`enrich_coach_context`].
#[derive(Debug, Clone, Copy)]
pub struct CoachEnrichOpts<'a> {
    /// When true, attach today's planned session (for `/status`). Trends should pass false.
    pub include_today_plan: bool,
    /// Load exercise blurbs for a single civil day (YYYY-MM-DD).
    pub exercise_date: Option<&'a str>,
    /// When set, load exercise blurbs for an inclusive date range (overrides single-day list).
    pub exercise_range: Option<(&'a str, &'a str)>,
}

impl<'a> CoachEnrichOpts<'a> {
    /// `/status`-style: today's plan + that day's exercises.
    pub fn for_day(exercise_date: &'a str) -> Self {
        Self {
            include_today_plan: true,
            exercise_date: Some(exercise_date),
            exercise_range: None,
        }
    }

    /// `/trends`-style: no today's plan; exercises across the trend window.
    pub fn for_trends(start: &'a str, end: &'a str) -> Self {
        Self {
            include_today_plan: false,
            exercise_date: None,
            exercise_range: Some((start, end)),
        }
    }
}

/// Enrich a day/trend coach context with fitness goals, plan progress, and exercises.
pub async fn enrich_coach_context(
    pool: &SqlitePool,
    config: &AppConfig,
    member_id: &str,
    ctx: FitnessCoachContext,
    opts: CoachEnrichOpts<'_>,
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
    let stored_plan = load_weekly_plan(pool, member_id, &week_start)
        .await
        .ok()
        .flatten();
    let parsed_plan: Option<WeeklyFitnessPlan> = stored_plan
        .as_ref()
        .and_then(|s| parse_plan_json(&s.plan_json).ok());

    let planned = if opts.include_today_plan {
        parsed_plan
            .as_ref()
            .and_then(|plan| session_for_date(&week_start, plan, today).cloned())
    } else {
        None
    };

    let exercises = if let Some((start, end)) = opts.exercise_range {
        exercise_entries_for_range(pool, member_id, start, end)
            .await
            .unwrap_or_default()
            .into_iter()
            .rev()
            .take(12)
            .map(|e| format!("{}: {}", e.date, e.description))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    } else if let Some(date) = opts.exercise_date {
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

    let (plan_matched, plan_planned, plan_cardio) = if let Some(ref plan) = parsed_plan {
        let label_rows: Vec<(String, String)> = week_ex
            .iter()
            .map(|e| (e.date.clone(), e.activity_label()))
            .collect();
        let (matched, planned_n) =
            plan_session_adherence(&week_start_s, plan, today, &label_rows);
        let duration_rows: Vec<(String, String, i32)> = week_ex
            .iter()
            .map(|e| (e.date.clone(), e.activity_label(), e.duration_mins()))
            .collect();
        let cardio_on_plan =
            plan_cardio_minutes_on_cardio_days(&week_start_s, plan, today, &duration_rows);
        (
            Some(matched),
            Some(planned_n),
            Some(cardio_on_plan),
        )
    } else {
        (None, None, None)
    };

    ctx.with_fitness(
        fitness,
        days_until,
        planned.as_ref(),
        exercises,
        Some(strength_done),
        strength_target,
        Some(cardio_done),
        cardio_target,
        plan_matched,
        plan_planned,
        plan_cardio,
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

/// One-line week progress for `/plan` replies.
pub fn format_plan_progress_line(
    matched: i32,
    planned: i32,
    week_cardio_minutes: i32,
    plan_cardio_minutes: i32,
) -> String {
    let mut parts = Vec::new();
    if planned > 0 {
        parts.push(format!("{}/{} sessions matched", matched, planned));
    } else {
        parts.push("no sessions planned yet this week".to_string());
    }
    parts.push(format!("{} min cardio logged", week_cardio_minutes));
    if plan_cardio_minutes > 0 || planned > 0 {
        parts.push(format!(
            "{} min on planned cardio/mixed days",
            plan_cardio_minutes
        ));
    }
    format!("📊 This week so far: {}", parts.join(" · "))
}

/// Load this week's exercises and format adherence for `/plan` and the morning brief.
pub async fn plan_week_progress_line(
    pool: &SqlitePool,
    member_id: &str,
    week_start: &str,
    plan_json: &str,
    as_of: NaiveDate,
) -> Option<String> {
    let plan = parse_plan_json(plan_json).ok()?;
    let week_end = (week_start_monday(as_of) + Duration::days(6))
        .format("%Y-%m-%d")
        .to_string();
    let week_ex = exercise_entries_for_range(pool, member_id, week_start, &week_end)
        .await
        .ok()?;
    let labels: Vec<(String, String)> = week_ex
        .iter()
        .map(|e| (e.date.clone(), e.activity_label()))
        .collect();
    let (matched, planned) = plan_session_adherence(week_start, &plan, as_of, &labels);
    let week_cardio = sum_cardio_minutes(
        week_ex
            .iter()
            .map(|e| (e.activity_label(), e.duration_mins())),
    );
    let duration_rows: Vec<(String, String, i32)> = week_ex
        .iter()
        .map(|e| (e.date.clone(), e.activity_label(), e.duration_mins()))
        .collect();
    let plan_cardio =
        plan_cardio_minutes_on_cardio_days(week_start, &plan, as_of, &duration_rows);
    Some(format_plan_progress_line(
        matched,
        planned,
        week_cardio,
        plan_cardio,
    ))
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

    #[test]
    fn plan_progress_line_formats_counts() {
        let line = format_plan_progress_line(2, 3, 45, 30);
        assert!(line.contains("2/3 sessions matched"));
        assert!(line.contains("45 min cardio logged"));
        assert!(line.contains("30 min on planned cardio/mixed days"));
    }

    #[test]
    fn enrich_opts_day_vs_trends() {
        let day = CoachEnrichOpts::for_day("2026-08-20");
        assert!(day.include_today_plan);
        assert_eq!(day.exercise_date, Some("2026-08-20"));
        assert!(day.exercise_range.is_none());

        let trends = CoachEnrichOpts::for_trends("2026-08-14", "2026-08-20");
        assert!(!trends.include_today_plan);
        assert!(trends.exercise_date.is_none());
        assert_eq!(
            trends.exercise_range,
            Some(("2026-08-14", "2026-08-20"))
        );
    }
}
