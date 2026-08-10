//! Weekly training plan generation and SQLite persistence.

use anyhow::{bail, Context, Result};
use chrono::{Datelike, Duration, Local, NaiveDate, Weekday};
use chotu_common::{AppConfig, ChotuLlm, FitnessGoals, NutritionGoals};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

const PLAN_SYSTEM_PROMPT: &str = "\
You are Chotu's household fitness coach. Build ONE week of training for a family member.\
Ground ONLY in the goals, constraints, and recent metrics provided — never invent logged numbers.\
Respect user constraints exactly. Do not diagnose medical conditions or prescribe rehab.\
Prefer progressive, realistic sessions for the stated equipment and session length.\
If recent exercise history is thin, keep the week beginner-friendly.\
Return a structured week with exactly 7 days (Monday through Sunday).\
Each day kind must be one of: rest, strength, cardio, mixed.\
Keep notes short (one sentence) and actionable.";

/// Session kind for a plan day.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PlanDayKind {
    Rest,
    Strength,
    Cardio,
    Mixed,
}

impl PlanDayKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanDayKind::Rest => "rest",
            PlanDayKind::Strength => "strength",
            PlanDayKind::Cardio => "cardio",
            PlanDayKind::Mixed => "mixed",
        }
    }

    pub fn parse_loose(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "strength" | "weights" | "lift" | "lifting" => PlanDayKind::Strength,
            "cardio" | "run" | "bike" | "endurance" => PlanDayKind::Cardio,
            "mixed" | "hybrid" | "both" => PlanDayKind::Mixed,
            _ => PlanDayKind::Rest,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct PlanDay {
    /// Monday .. Sunday
    pub weekday: String,
    pub kind: PlanDayKind,
    /// Short session note (empty ok for rest).
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct WeeklyFitnessPlan {
    pub days: Vec<PlanDay>,
    /// One-line week theme.
    #[serde(default)]
    pub theme: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredWeeklyPlan {
    pub family_member_id: String,
    pub week_start: String,
    pub plan_md: String,
    pub plan_json: String,
    pub created_at: String,
}

/// Monday (ISO week) for the local civil day containing `date`.
pub fn week_start_monday(date: NaiveDate) -> NaiveDate {
    let wd = date.weekday();
    let days_from_monday = wd.num_days_from_monday() as i64;
    date - Duration::days(days_from_monday)
}

/// Monday of the current local week as YYYY-MM-DD.
pub fn current_week_start_str() -> String {
    week_start_monday(Local::now().date_naive())
        .format("%Y-%m-%d")
        .to_string()
}

pub async fn load_weekly_plan(
    pool: &SqlitePool,
    member_id: &str,
    week_start: &str,
) -> Result<Option<StoredWeeklyPlan>> {
    let row = sqlx::query_as::<_, StoredWeeklyPlan>(
        "SELECT family_member_id, week_start, plan_md, plan_json, created_at \
         FROM fitness_weekly_plans \
         WHERE family_member_id = ? AND week_start = ?",
    )
    .bind(member_id)
    .bind(week_start)
    .fetch_optional(pool)
    .await
    .context("Failed to load fitness_weekly_plans row")?;
    Ok(row)
}

pub async fn save_weekly_plan(
    pool: &SqlitePool,
    member_id: &str,
    week_start: &str,
    plan: &WeeklyFitnessPlan,
) -> Result<StoredWeeklyPlan> {
    let plan_json = serde_json::to_string(plan).context("serialize weekly plan")?;
    let plan_md = render_plan_markdown(week_start, plan);
    sqlx::query(
        "INSERT INTO fitness_weekly_plans (family_member_id, week_start, plan_md, plan_json, created_at) \
         VALUES (?, ?, ?, ?, datetime('now')) \
         ON CONFLICT(family_member_id, week_start) DO UPDATE SET \
           plan_md = excluded.plan_md, \
           plan_json = excluded.plan_json, \
           created_at = excluded.created_at",
    )
    .bind(member_id)
    .bind(week_start)
    .bind(&plan_md)
    .bind(&plan_json)
    .execute(pool)
    .await
    .context("Failed to upsert fitness_weekly_plans")?;

    Ok(StoredWeeklyPlan {
        family_member_id: member_id.to_string(),
        week_start: week_start.to_string(),
        plan_md,
        plan_json,
        created_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Parse stored JSON, tolerating minor LLM drift via [`normalize_weekly_plan`].
pub fn parse_plan_json(raw: &str) -> Result<WeeklyFitnessPlan> {
    let plan: WeeklyFitnessPlan =
        serde_json::from_str(raw).context("Failed to parse weekly plan JSON")?;
    normalize_weekly_plan(plan)
}

/// Ensure 7 Mon–Sun days; fill missing with rest.
pub fn normalize_weekly_plan(mut plan: WeeklyFitnessPlan) -> Result<WeeklyFitnessPlan> {
    const ORDER: [&str; 7] = [
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ];
    let mut by_day: std::collections::HashMap<String, PlanDay> = std::collections::HashMap::new();
    for day in plan.days.drain(..) {
        let key = normalize_weekday_name(&day.weekday);
        by_day.insert(
            key.clone(),
            PlanDay {
                weekday: key,
                kind: day.kind,
                notes: day.notes,
            },
        );
    }
    let mut days = Vec::with_capacity(7);
    for name in ORDER {
        days.push(by_day.remove(name).unwrap_or(PlanDay {
            weekday: name.to_string(),
            kind: PlanDayKind::Rest,
            notes: String::new(),
        }));
    }
    plan.days = days;
    Ok(plan)
}

fn normalize_weekday_name(raw: &str) -> String {
    let t = raw.trim().to_lowercase();
    match t.as_str() {
        "mon" | "monday" => "Monday".to_string(),
        "tue" | "tues" | "tuesday" => "Tuesday".to_string(),
        "wed" | "wednesday" => "Wednesday".to_string(),
        "thu" | "thur" | "thurs" | "thursday" => "Thursday".to_string(),
        "fri" | "friday" => "Friday".to_string(),
        "sat" | "saturday" => "Saturday".to_string(),
        "sun" | "sunday" => "Sunday".to_string(),
        _ => {
            // Title-case fallback
            let mut c = raw.trim().chars();
            match c.next() {
                None => "Monday".to_string(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        }
    }
}

pub fn render_plan_markdown(week_start: &str, plan: &WeeklyFitnessPlan) -> String {
    let mut out = format!("🏋️ *Training plan* (week of {})\n", week_start);
    if !plan.theme.trim().is_empty() {
        out.push_str(&format!("_{}_\n", plan.theme.trim()));
    }
    out.push('\n');
    for day in &plan.days {
        let note = day.notes.trim();
        if note.is_empty() {
            out.push_str(&format!("• *{}*: {}\n", day.weekday, day.kind.as_str()));
        } else {
            out.push_str(&format!(
                "• *{}*: {} — {}\n",
                day.weekday,
                day.kind.as_str(),
                note
            ));
        }
    }
    out
}

/// Session planned for a civil date within `plan` (week starting Monday).
pub fn session_for_date<'a>(
    week_start: &str,
    plan: &'a WeeklyFitnessPlan,
    date: NaiveDate,
) -> Option<&'a PlanDay> {
    let start = NaiveDate::parse_from_str(week_start, "%Y-%m-%d").ok()?;
    let offset = (date - start).num_days();
    if !(0..=6).contains(&offset) {
        return None;
    }
    plan.days.get(offset as usize)
}

pub fn session_for_date_from_stored(
    stored: &StoredWeeklyPlan,
    date: NaiveDate,
) -> Option<PlanDay> {
    let plan = parse_plan_json(&stored.plan_json).ok()?;
    session_for_date(&stored.week_start, &plan, date).cloned()
}

/// Build Ollama user prompt from config + recent averages/exercises.
pub fn build_plan_user_prompt(
    member_name: &str,
    fitness: &FitnessGoals,
    nutrition: Option<&NutritionGoals>,
    week_start: &str,
    avg_calories: f64,
    avg_protein: f64,
    avg_steps: f64,
    avg_active: f64,
    recent_exercises: &[(String, String)],
) -> String {
    let mut lines = Vec::new();
    lines.push(format!("Member: {}", member_name));
    lines.push(format!("Week starting Monday: {}", week_start));
    if let Some(intent) = fitness.intent.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        lines.push(format!("Outcome intent: {}", intent));
    }
    if let Some(td) = fitness
        .target_date
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        lines.push(format!("Target date: {}", td));
        if let Ok(as_of) = NaiveDate::parse_from_str(week_start, "%Y-%m-%d") {
            if let Some(days) = fitness.days_until_target(as_of) {
                lines.push(format!("Days until target (from week start): {}", days));
            }
        }
    }
    if let Some(focus) = fitness.focus.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        lines.push(format!("Focus: {}", focus));
    }
    if let Some(n) = fitness.sessions_per_week {
        lines.push(format!("Sessions per week: {}", n));
    }
    if let Some(m) = fitness.session_minutes {
        lines.push(format!("Session minutes: {}", m));
    }
    if let Some(eq) = fitness
        .equipment
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        lines.push(format!("Equipment: {}", eq));
    }
    if !fitness.constraints.is_empty() {
        lines.push("Constraints:".to_string());
        for c in &fitness.constraints {
            let t = c.trim();
            if !t.is_empty() {
                lines.push(format!("  - {}", t));
            }
        }
    }
    if let Some(wt) = fitness.weekly_targets.as_ref() {
        lines.push("Weekly targets:".to_string());
        if let Some(s) = wt.strength_sessions {
            lines.push(format!("  - strength_sessions: {}", s));
        }
        if let Some(m) = wt.cardio_minutes {
            lines.push(format!("  - cardio_minutes: {}", m));
        }
        if let Some(a) = wt.active_calories {
            lines.push(format!("  - active_calories (daily floor): {}", a));
        }
    }
    if let Some(ng) = nutrition {
        lines.push("Daily nutrition goals:".to_string());
        if let Some(c) = ng.calories {
            lines.push(format!("  - calories: {}", c));
        }
        if let Some(p) = ng.protein_g {
            lines.push(format!("  - protein_g: {}", p));
        }
        if let Some(s) = ng.steps {
            lines.push(format!("  - steps: {}", s));
        }
    }
    lines.push("Recent 7-day averages (logged):".to_string());
    lines.push(format!("  - calories: {:.0}", avg_calories));
    lines.push(format!("  - protein_g: {:.1}", avg_protein));
    lines.push(format!("  - steps: {:.0}", avg_steps));
    lines.push(format!("  - active_calories: {:.0}", avg_active));
    if recent_exercises.is_empty() {
        lines.push("Recent exercises: none logged".to_string());
    } else {
        lines.push("Recent exercises:".to_string());
        for (date, desc) in recent_exercises.iter().take(20) {
            lines.push(format!("  - {}: {}", date, desc));
        }
    }
    lines.push(
        "Produce a 7-day plan (Monday–Sunday) matching sessions_per_week and constraints."
            .to_string(),
    );
    lines.join("\n")
}

/// Generate (or regenerate) this week's plan for a member and persist it.
pub async fn generate_and_store_weekly_plan(
    pool: &SqlitePool,
    llm: &ChotuLlm,
    config: &AppConfig,
    member_id: &str,
    week_start: &str,
) -> Result<StoredWeeklyPlan> {
    let member = config
        .family
        .members
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(member_id))
        .with_context(|| format!("Unknown member id '{}'", member_id))?;
    let fitness = member
        .fitness_goals
        .as_ref()
        .filter(|g| !g.is_empty())
        .with_context(|| {
            format!(
                "Member '{}' has no fitness_goals in config.yaml — add them first",
                member_id
            )
        })?;

    let start = NaiveDate::parse_from_str(week_start, "%Y-%m-%d")
        .context("week_start must be YYYY-MM-DD")?;
    let hist_start = (start - Duration::days(7)).format("%Y-%m-%d").to_string();
    let hist_end = (start - Duration::days(1)).format("%Y-%m-%d").to_string();

    let (avg_cal, avg_protein, avg_steps, avg_active) =
        week_averages(pool, member_id, &hist_start, &hist_end).await?;
    let recent = crate::sync::exercises_for_range(pool, member_id, &hist_start, &hist_end)
        .await
        .context("Failed to load recent exercises for weekly plan")?;

    let user_prompt = build_plan_user_prompt(
        &member.name,
        fitness,
        member.nutrition_goals.as_ref(),
        week_start,
        avg_cal,
        avg_protein,
        avg_steps,
        avg_active,
        &recent,
    );

    let plan = match llm
        .extract_typed::<WeeklyFitnessPlan>(PLAN_SYSTEM_PROMPT, &user_prompt)
        .await
    {
        Ok(p) => normalize_weekly_plan(p)?,
        Err(e) => {
            eprintln!(
                "Fitness plan structured extract failed ({:?}); trying JSON text fallback",
                e
            );
            let raw = llm
                .generate_prompt_fast(
                    &(PLAN_SYSTEM_PROMPT.to_string()
                        + " Reply with JSON only: {\"theme\":\"...\",\"days\":[{\"weekday\":\"Monday\",\"kind\":\"strength\",\"notes\":\"...\"}, ... 7 days]}"),
                    &user_prompt,
                )
                .await
                .context("Ollama plan generation failed")?;
            let json = extract_json_object(&raw).context("No JSON object in plan response")?;
            normalize_weekly_plan(parse_plan_json(json)?)?
        }
    };

    if plan.days.len() != 7 {
        bail!("plan must have 7 days after normalize");
    }
    save_weekly_plan(pool, &member.id, week_start, &plan).await
}

async fn week_averages(
    pool: &SqlitePool,
    member_id: &str,
    start: &str,
    end: &str,
) -> Result<(f64, f64, f64, f64)> {
    #[derive(sqlx::FromRow)]
    struct Row {
        avg_cal: Option<f64>,
        avg_protein: Option<f64>,
        avg_steps: Option<f64>,
        avg_active: Option<f64>,
    }
    let row: Row = sqlx::query_as(
        "SELECT AVG(total_calories_ingested) as avg_cal, \
                AVG(protein_grams) as avg_protein, \
                AVG(step_count) as avg_steps, \
                AVG(active_calories_burned) as avg_active \
         FROM health_family_summary \
         WHERE family_member_id = ? AND date >= ? AND date <= ?",
    )
    .bind(member_id)
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await
    .context("Failed to average health_family_summary for plan")?;
    Ok((
        row.avg_cal.unwrap_or(0.0),
        row.avg_protein.unwrap_or(0.0),
        row.avg_steps.unwrap_or(0.0),
        row.avg_active.unwrap_or(0.0),
    ))
}

/// Pull the first `{...}` object from model text (strips fences).
pub fn extract_json_object(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let body = if let Some(rest) = trimmed.strip_prefix("```json") {
        rest.strip_suffix("```").unwrap_or(rest).trim()
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        rest.strip_suffix("```").unwrap_or(rest).trim()
    } else {
        trimmed
    };
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    if end < start {
        return None;
    }
    Some(&body[start..=end])
}

/// Count strength-like exercise descriptions in a week (heuristic for coach progress).
pub fn count_strengthish_sessions(descriptions: &[String]) -> i32 {
    descriptions
        .iter()
        .filter(|d| {
            let l = d.to_lowercase();
            l.contains("strength")
                || l.contains("weight")
                || l.contains("lift")
                || l.contains("gym")
                || l.contains("workout")
                || l.contains("functional")
                || l.contains("hiit")
        })
        .count() as i32
}

/// Weekday name for a NaiveDate (Monday..Sunday).
pub fn weekday_name(date: NaiveDate) -> &'static str {
    match date.weekday() {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn week_start_aligns_to_monday() {
        // 2026-08-09 is Sunday
        let sun = NaiveDate::from_ymd_opt(2026, 8, 9).unwrap();
        assert_eq!(
            week_start_monday(sun),
            NaiveDate::from_ymd_opt(2026, 8, 3).unwrap()
        );
        let mon = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        assert_eq!(week_start_monday(mon), mon);
        let wed = NaiveDate::from_ymd_opt(2026, 8, 5).unwrap();
        assert_eq!(week_start_monday(wed), mon);
    }

    #[test]
    fn normalize_fills_missing_days() {
        let plan = WeeklyFitnessPlan {
            theme: "Beach prep".into(),
            days: vec![PlanDay {
                weekday: "mon".into(),
                kind: PlanDayKind::Strength,
                notes: "squats".into(),
            }],
        };
        let n = normalize_weekly_plan(plan).unwrap();
        assert_eq!(n.days.len(), 7);
        assert_eq!(n.days[0].kind, PlanDayKind::Strength);
        assert_eq!(n.days[1].kind, PlanDayKind::Rest);
        assert_eq!(n.days[6].weekday, "Sunday");
    }

    #[test]
    fn parse_and_session_for_date() {
        let json = r#"{
            "theme": "Base week",
            "days": [
                {"weekday":"Monday","kind":"strength","notes":"upper"},
                {"weekday":"Tuesday","kind":"rest","notes":""},
                {"weekday":"Wednesday","kind":"cardio","notes":"bike 30"},
                {"weekday":"Thursday","kind":"strength","notes":"lower"},
                {"weekday":"Friday","kind":"rest","notes":""},
                {"weekday":"Saturday","kind":"mixed","notes":"circuit"},
                {"weekday":"Sunday","kind":"rest","notes":""}
            ]
        }"#;
        let plan = parse_plan_json(json).unwrap();
        let week = "2026-08-03";
        let wed = NaiveDate::from_ymd_opt(2026, 8, 5).unwrap();
        let s = session_for_date(week, &plan, wed).unwrap();
        assert_eq!(s.kind, PlanDayKind::Cardio);
        assert!(s.notes.contains("bike"));
    }

    #[test]
    fn extract_json_from_fenced() {
        let raw = "```json\n{\"theme\":\"x\",\"days\":[]}\n```";
        let obj = extract_json_object(raw).unwrap();
        assert!(obj.contains("theme"));
    }

    #[test]
    fn plan_user_prompt_includes_constraints() {
        let fitness = FitnessGoals {
            intent: Some("beach body".into()),
            target_date: Some("2027-06-01".into()),
            focus: Some("recomp".into()),
            sessions_per_week: Some(4),
            session_minutes: Some(45),
            equipment: Some("gym".into()),
            constraints: vec!["no hard runs".into()],
            weekly_targets: None,
        };
        let prompt = build_plan_user_prompt(
            "Alex",
            &fitness,
            None,
            "2026-08-03",
            2000.0,
            120.0,
            8000.0,
            350.0,
            &[("2026-08-01".into(), "Weights 40m".into())],
        );
        assert!(prompt.contains("beach body"));
        assert!(prompt.contains("no hard runs"));
        assert!(prompt.contains("Weights 40m"));
    }
}
