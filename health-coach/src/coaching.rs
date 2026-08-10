//! Short local-Ollama coaching tips grounded in metrics, goals, and training plans.

use anyhow::{Context, Result};
use chotu_common::{ChotuLlm, FitnessGoals, HealthFamilySummary, NutritionGoals};

use crate::fitness_plan::PlanDay;

const COACH_SYSTEM_PROMPT: &str = "You are Chotu's household fitness and nutrition coach. \
Write exactly 1–2 short sentences of coaching for one family member. \
Ground ONLY in the metrics, goals, plan, and constraints provided — never invent numbers or medical diagnoses. \
When an outcome target date is set and they are off-track on food, steps, or today's planned session, briefly reference the timeline (e.g. days remaining) plus one concrete nudge. \
If they are broadly on track, briefly celebrate a concrete win. \
Respect listed constraints. No preamble, no bullet lists, no emoji spam. Plain text suitable for Telegram (light Markdown ok). \
Return only the tip text.";

/// Structured snapshot fed to the coach LLM (nutrition + fitness outcome).
#[derive(Debug, Clone)]
pub struct FitnessCoachContext {
    pub member_name: String,
    /// e.g. "today (2026-08-04)" or "last 7 days (5 logged)"
    pub window_label: String,
    pub calories: f64,
    pub protein_g: f64,
    pub carbs_g: f64,
    pub fats_g: f64,
    pub fiber_g: f64,
    pub steps: f64,
    pub active_calories: f64,
    pub sleep_hours: Option<f64>,
    pub perceived_energy: Option<i32>,
    pub goals: Option<NutritionGoals>,
    pub fitness_goals: Option<FitnessGoals>,
    /// Days until fitness target_date (from local today when built).
    pub days_until_target: Option<i64>,
    /// Today's planned session summary, if any.
    pub planned_session: Option<String>,
    /// Free-text exercise blurbs for the window (v1).
    /// TODO: replace with structured exercise rows (activity type, duration,
    /// calories, start/end) once `exercise_log` schema is expanded beyond
    /// description strings — see TODO.md.
    pub exercises: Vec<String>,
    /// Strength-like sessions logged this week vs weekly target.
    pub week_strength_sessions: Option<i32>,
    pub week_strength_target: Option<i32>,
    /// Optional trend arrows for calories / protein / steps (↑ ↓ →).
    pub calorie_trend: Option<&'static str>,
    pub protein_trend: Option<&'static str>,
    pub steps_trend: Option<&'static str>,
}

/// Backward-compatible name used by `/status` and `/trends`.
pub type NutritionCoachContext = FitnessCoachContext;

impl FitnessCoachContext {
    /// True when there is something worth coaching on (any intake, activity, or plan).
    pub fn has_health_data(&self) -> bool {
        self.calories > 0.0
            || self.protein_g > 0.0
            || self.carbs_g > 0.0
            || self.fats_g > 0.0
            || self.fiber_g > 0.0
            || self.steps > 0.0
            || self.active_calories > 0.0
            || self.sleep_hours.is_some()
            || self.perceived_energy.is_some()
            || !self.exercises.is_empty()
            || self.planned_session.is_some()
            || self
                .fitness_goals
                .as_ref()
                .map(|g| !g.is_empty())
                .unwrap_or(false)
    }

    /// Build context from a single day's health summary.
    pub fn from_day_summary(
        member_name: &str,
        summary: &HealthFamilySummary,
        goals: Option<&NutritionGoals>,
    ) -> Self {
        Self {
            member_name: member_name.to_string(),
            window_label: format!("today ({})", summary.date),
            calories: summary.total_calories_ingested as f64,
            protein_g: summary.protein_grams,
            carbs_g: summary.carbs_grams,
            fats_g: summary.fats_grams,
            fiber_g: summary.fiber_g,
            steps: summary.step_count as f64,
            active_calories: summary.active_calories_burned as f64,
            sleep_hours: summary.sleep_hours,
            perceived_energy: summary.perceived_energy,
            goals: goals.cloned(),
            fitness_goals: None,
            days_until_target: None,
            planned_session: None,
            exercises: Vec::new(),
            week_strength_sessions: None,
            week_strength_target: None,
            calorie_trend: None,
            protein_trend: None,
            steps_trend: None,
        }
    }

    /// Attach fitness outcome, plan session, and exercises.
    pub fn with_fitness(
        mut self,
        fitness: Option<&FitnessGoals>,
        days_until_target: Option<i64>,
        planned: Option<&PlanDay>,
        exercises: Vec<String>,
        week_strength_sessions: Option<i32>,
        week_strength_target: Option<i32>,
    ) -> Self {
        self.fitness_goals = fitness.cloned();
        self.days_until_target = days_until_target;
        self.planned_session = planned.map(|p| {
            let notes = p.notes.trim();
            if notes.is_empty() {
                format!("{} ({})", p.weekday, p.kind.as_str())
            } else {
                format!("{} ({}): {}", p.weekday, p.kind.as_str(), notes)
            }
        });
        self.exercises = exercises;
        self.week_strength_sessions = week_strength_sessions;
        self.week_strength_target = week_strength_target;
        self
    }

    /// Build context from multi-day averages (trends).
    pub fn from_trend_averages(
        member_name: &str,
        days: i64,
        logged_days: usize,
        avg_cal: f64,
        avg_protein: f64,
        avg_carbs: f64,
        avg_fats: f64,
        avg_fiber: f64,
        avg_steps: f64,
        avg_sleep: Option<f64>,
        goals: Option<&NutritionGoals>,
        calorie_trend: &'static str,
        protein_trend: &'static str,
        steps_trend: &'static str,
    ) -> Self {
        Self {
            member_name: member_name.to_string(),
            window_label: format!("last {} days ({} logged)", days, logged_days),
            calories: avg_cal,
            protein_g: avg_protein,
            carbs_g: avg_carbs,
            fats_g: avg_fats,
            fiber_g: avg_fiber,
            steps: avg_steps,
            active_calories: 0.0,
            sleep_hours: avg_sleep,
            perceived_energy: None,
            goals: goals.cloned(),
            fitness_goals: None,
            days_until_target: None,
            planned_session: None,
            exercises: Vec::new(),
            week_strength_sessions: None,
            week_strength_target: None,
            calorie_trend: Some(calorie_trend),
            protein_trend: Some(protein_trend),
            steps_trend: Some(steps_trend),
        }
    }

    /// Plain-text user prompt for the LLM.
    pub fn to_user_prompt(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("Member: {}", self.member_name));
        lines.push(format!("Window: {}", self.window_label));
        lines.push(format!(
            "Calories: {:.0} kcal{}",
            self.calories,
            trend_suffix(self.calorie_trend)
        ));
        lines.push(format!(
            "Protein: {:.1}g{}",
            self.protein_g,
            trend_suffix(self.protein_trend)
        ));
        lines.push(format!("Carbs: {:.1}g", self.carbs_g));
        lines.push(format!("Fat: {:.1}g", self.fats_g));
        lines.push(format!("Fiber: {:.1}g", self.fiber_g));
        lines.push(format!(
            "Steps: {:.0}{}",
            self.steps,
            trend_suffix(self.steps_trend)
        ));
        if self.active_calories > 0.0 {
            lines.push(format!("Active calories: {:.0}", self.active_calories));
        }
        if let Some(sleep) = self.sleep_hours {
            lines.push(format!("Sleep: {:.1} hours", sleep));
        }
        if let Some(energy) = self.perceived_energy {
            lines.push(format!("Perceived energy: {}/10", energy));
        }

        if let Some(goals) = self.goals.as_ref().filter(|g| !g.is_empty()) {
            lines.push("Nutrition goals vs actual:".to_string());
            if let Some(g) = goals.calories {
                lines.push(goal_line("Calories", self.calories, g as f64, "kcal"));
            }
            if let Some(g) = goals.protein_g {
                lines.push(goal_line("Protein", self.protein_g, g, "g"));
            }
            if let Some(g) = goals.carbs_g {
                lines.push(goal_line("Carbs", self.carbs_g, g, "g"));
            }
            if let Some(g) = goals.fats_g {
                lines.push(goal_line("Fat", self.fats_g, g, "g"));
            }
            if let Some(g) = goals.fiber_g {
                lines.push(goal_line("Fiber", self.fiber_g, g, "g"));
            }
            if let Some(g) = goals.steps {
                lines.push(goal_line("Steps", self.steps, g as f64, ""));
            }
        } else {
            lines.push("Nutrition goals: none configured".to_string());
        }

        if let Some(fg) = self.fitness_goals.as_ref().filter(|g| !g.is_empty()) {
            lines.push("Fitness outcome:".to_string());
            if let Some(intent) = fg.intent.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                lines.push(format!("  - Intent: {}", intent));
            }
            if let Some(td) = fg
                .target_date
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                lines.push(format!("  - Target date: {}", td));
            }
            if let Some(days) = self.days_until_target {
                lines.push(format!("  - Days remaining: {}", days));
            }
            if let Some(focus) = fg.focus {
                lines.push(format!("  - Focus: {}", focus.as_str()));
            }
            if let Some(wt) = fg.weekly_targets.as_ref() {
                if let Some(target) = wt.active_calories {
                    lines.push(goal_line(
                        "Active calories",
                        self.active_calories,
                        target as f64,
                        "kcal",
                    ));
                }
            }
            if !fg.constraints.is_empty() {
                lines.push("Constraints:".to_string());
                for c in &fg.constraints {
                    let t = c.trim();
                    if !t.is_empty() {
                        lines.push(format!("  - {}", t));
                    }
                }
            }
        }

        if let Some(session) = &self.planned_session {
            lines.push(format!("Planned session today: {}", session));
        }
        if self.exercises.is_empty() {
            lines.push("Exercises logged: none".to_string());
        } else {
            lines.push("Exercises logged:".to_string());
            for e in &self.exercises {
                lines.push(format!("  - {}", e));
            }
        }
        if let (Some(done), Some(target)) = (self.week_strength_sessions, self.week_strength_target)
        {
            lines.push(format!(
                "Strength-like sessions this week: {}/{}",
                done, target
            ));
        }

        lines.join("\n")
    }
}

fn trend_suffix(trend: Option<&'static str>) -> String {
    match trend {
        Some(t) => format!(" ({})", t),
        None => String::new(),
    }
}

fn goal_line(label: &str, actual: f64, goal: f64, unit: &str) -> String {
    let pct = if goal > 0.0 {
        (actual / goal) * 100.0
    } else {
        0.0
    };
    if unit.is_empty() {
        format!("  - {}: {:.0}/{:.0} ({:.0}%)", label, actual, goal, pct)
    } else {
        format!(
            "  - {}: {:.0}/{:.0}{} ({:.0}%)",
            label, actual, goal, unit, pct
        )
    }
}

/// Ask local Ollama for a short coaching tip. Caller should skip when `!ctx.has_health_data()`.
pub async fn generate_nutrition_coach_tip(
    llm: &ChotuLlm,
    ctx: &FitnessCoachContext,
) -> Result<String> {
    generate_fitness_coach_tip(llm, ctx).await
}

pub async fn generate_fitness_coach_tip(
    llm: &ChotuLlm,
    ctx: &FitnessCoachContext,
) -> Result<String> {
    let user_prompt = ctx.to_user_prompt();
    let raw = llm
        .generate_prompt_fast(COACH_SYSTEM_PROMPT, &user_prompt)
        .await
        .context("Ollama coach tip generation failed")?;
    let cleaned = strip_think_blocks(&raw).trim().to_string();
    if cleaned.is_empty() {
        anyhow::bail!("coach tip was empty after cleaning");
    }
    Ok(cleaned)
}

/// Append a `• *Coach:* …` block when generation succeeds; otherwise leave `report` unchanged.
pub async fn append_coach_tip(llm: &ChotuLlm, ctx: &FitnessCoachContext, report: &mut String) {
    if !ctx.has_health_data() {
        return;
    }
    match generate_fitness_coach_tip(llm, ctx).await {
        Ok(tip) => {
            report.push_str("\n• *Coach:* ");
            report.push_str(&tip);
            if !tip.ends_with('\n') {
                report.push('\n');
            }
        }
        Err(e) => {
            eprintln!("Fitness coach tip failed for {}: {:?}", ctx.member_name, e);
        }
    }
}

fn strip_think_blocks(text: &str) -> String {
    let mut output = String::new();
    let mut remaining = text;
    while let Some(start_idx) = remaining.find("<think>") {
        output.push_str(&remaining[..start_idx]);
        let after_open = &remaining[start_idx + "<think>".len()..];
        if let Some(end_idx) = after_open.find("</think>") {
            remaining = &after_open[end_idx + "</think>".len()..];
        } else {
            // Unclosed think tag — drop the rest.
            remaining = "";
            break;
        }
    }
    output.push_str(remaining);
    output.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chotu_common::{FitnessGoals, NutritionGoals};
    use crate::fitness_plan::{PlanDay, PlanDayKind};

    fn sample_summary() -> HealthFamilySummary {
        HealthFamilySummary {
            date: "2026-08-04".to_string(),
            family_member_id: "praj".to_string(),
            total_calories_ingested: 1800,
            protein_grams: 90.0,
            carbs_grams: 200.0,
            fats_grams: 60.0,
            step_count: 7500,
            active_calories_burned: 400,
            sleep_hours: Some(7.0),
            perceived_energy: Some(7),
            omega_3_dha_mg: 0.0,
            cholesterol_mg: 0.0,
            saturated_fat_g: 0.0,
            unsaturated_fat_g: 0.0,
            triglycerides_mg: 0.0,
            iron_mg: 0.0,
            vitamin_b_mg: 0.0,
            vitamin_c_mg: 0.0,
            sugar_g: 0.0,
            fiber_g: 20.0,
            sodium_mg: 0.0,
            potassium_mg: 0.0,
            calcium_mg: 0.0,
            magnesium_mg: 0.0,
            zinc_mg: 0.0,
            vitamin_a_mcg: 0.0,
            vitamin_d_mcg: 0.0,
            vitamin_e_mg: 0.0,
            vitamin_k_mcg: 0.0,
            caffeine_mg: 0.0,
            trans_fat_g: 0.0,
        }
    }

    #[test]
    fn has_health_data_false_when_empty() {
        let mut s = sample_summary();
        s.total_calories_ingested = 0;
        s.protein_grams = 0.0;
        s.carbs_grams = 0.0;
        s.fats_grams = 0.0;
        s.fiber_g = 0.0;
        s.step_count = 0;
        s.active_calories_burned = 0;
        s.sleep_hours = None;
        s.perceived_energy = None;
        let ctx = FitnessCoachContext::from_day_summary("Praj", &s, None);
        assert!(!ctx.has_health_data());
    }

    #[test]
    fn has_health_data_true_with_calories() {
        let ctx = FitnessCoachContext::from_day_summary("Praj", &sample_summary(), None);
        assert!(ctx.has_health_data());
    }

    #[test]
    fn user_prompt_includes_goals_and_percent() {
        let goals = NutritionGoals {
            calories: Some(2000),
            protein_g: Some(150.0),
            carbs_g: None,
            fats_g: None,
            fiber_g: Some(30.0),
            steps: Some(10000),
        };
        let ctx = FitnessCoachContext::from_day_summary("Praj", &sample_summary(), Some(&goals));
        let prompt = ctx.to_user_prompt();
        assert!(prompt.contains("Member: Praj"));
        assert!(prompt.contains("today (2026-08-04)"));
        assert!(prompt.contains("Calories: 1800/2000kcal (90%)"));
        assert!(prompt.contains("Protein: 90/150g (60%)"));
        assert!(prompt.contains("Steps: 7500/10000 (75%)"));
    }

    #[test]
    fn user_prompt_includes_fitness_outcome_and_plan() {
        let fitness = FitnessGoals {
            intent: Some("beach body".into()),
            target_date: Some("2027-06-01".into()),
            focus: Some(chotu_common::FitnessFocus::Recomp),
            sessions_per_week: Some(4),
            session_minutes: Some(45),
            equipment: Some(chotu_common::FitnessEquipment::Gym),
            constraints: vec!["low-impact cardio".into()],
            weekly_targets: Some(chotu_common::FitnessWeeklyTargets {
                strength_sessions: Some(3),
                cardio_minutes: None,
                active_calories: Some(400),
            }),
        };
        let planned = PlanDay {
            weekday: "Tuesday".into(),
            kind: PlanDayKind::Strength,
            notes: "lower body".into(),
        };
        let ctx = FitnessCoachContext::from_day_summary("Praj", &sample_summary(), None)
            .with_fitness(
                Some(&fitness),
                Some(295),
                Some(&planned),
                vec!["Weights 40m".into()],
                Some(1),
                Some(3),
            );
        let prompt = ctx.to_user_prompt();
        assert!(prompt.contains("Intent: beach body"));
        assert!(prompt.contains("Days remaining: 295"));
        assert!(prompt.contains("Planned session today: Tuesday (strength): lower body"));
        assert!(prompt.contains("Weights 40m"));
        assert!(prompt.contains("low-impact cardio"));
        assert!(prompt.contains("Strength-like sessions this week: 1/3"));
        assert!(prompt.contains("Active calories: 400/400kcal"));
    }

    #[test]
    fn trend_context_includes_arrows() {
        let ctx = FitnessCoachContext::from_trend_averages(
            "Alex",
            7,
            5,
            1900.0,
            100.0,
            210.0,
            55.0,
            18.0,
            8000.0,
            Some(6.5),
            None,
            "↑",
            "→",
            "↓",
        );
        let prompt = ctx.to_user_prompt();
        assert!(prompt.contains("last 7 days (5 logged)"));
        assert!(prompt.contains("Calories: 1900 kcal (↑)"));
        assert!(prompt.contains("Protein: 100.0g (→)"));
        assert!(prompt.contains("Steps: 8000 (↓)"));
        assert!(prompt.contains("Nutrition goals: none configured"));
    }

    #[test]
    fn strip_think_blocks_removes_reasoning() {
        assert_eq!(
            strip_think_blocks("<think>secret</think>Eat more protein today."),
            "Eat more protein today."
        );
    }

    #[test]
    fn strip_think_blocks_ignores_stray_close_before_open() {
        // Stray </think> before an open tag must not be used as the block closer.
        assert_eq!(
            strip_think_blocks("Keep this.</think><think>drop</think> Tip stays."),
            "Keep this.</think> Tip stays."
        );
    }
}
