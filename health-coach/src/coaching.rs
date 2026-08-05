//! Short local-Ollama nutrition coaching tips grounded in logged metrics/goals.

use anyhow::{Context, Result};
use chotu_common::{ChotuLlm, HealthFamilySummary, NutritionGoals};

const COACH_SYSTEM_PROMPT: &str = "You are Chotu's household nutrition coach. \
Write exactly 1–2 short sentences of coaching for one family member. \
Ground ONLY in the metrics and goals provided — never invent numbers or medical diagnoses. \
If they are broadly on track, briefly celebrate a concrete win. \
If something is off, give one gentle, concrete food or activity nudge. \
No preamble, no bullet lists, no emoji spam. Plain text suitable for Telegram (light Markdown ok). \
Return only the tip text.";

/// Structured snapshot fed to the coach LLM.
#[derive(Debug, Clone)]
pub struct NutritionCoachContext {
    pub member_name: String,
    /// e.g. "today (2026-08-04)" or "last 7 days (5 logged)"
    pub window_label: String,
    pub calories: f64,
    pub protein_g: f64,
    pub carbs_g: f64,
    pub fats_g: f64,
    pub fiber_g: f64,
    pub steps: f64,
    pub sleep_hours: Option<f64>,
    pub perceived_energy: Option<i32>,
    pub goals: Option<NutritionGoals>,
    /// Optional trend arrows for calories / protein / steps (↑ ↓ →).
    pub calorie_trend: Option<&'static str>,
    pub protein_trend: Option<&'static str>,
    pub steps_trend: Option<&'static str>,
}

impl NutritionCoachContext {
    /// True when there is something worth coaching on (any intake or activity).
    pub fn has_health_data(&self) -> bool {
        self.calories > 0.0
            || self.protein_g > 0.0
            || self.carbs_g > 0.0
            || self.fats_g > 0.0
            || self.fiber_g > 0.0
            || self.steps > 0.0
            || self.sleep_hours.is_some()
            || self.perceived_energy.is_some()
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
            sleep_hours: summary.sleep_hours,
            perceived_energy: summary.perceived_energy,
            goals: goals.cloned(),
            calorie_trend: None,
            protein_trend: None,
            steps_trend: None,
        }
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
            sleep_hours: avg_sleep,
            perceived_energy: None,
            goals: goals.cloned(),
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
        if let Some(sleep) = self.sleep_hours {
            lines.push(format!("Sleep: {:.1} hours", sleep));
        }
        if let Some(energy) = self.perceived_energy {
            lines.push(format!("Perceived energy: {}/10", energy));
        }

        if let Some(goals) = self.goals.as_ref().filter(|g| !g.is_empty()) {
            lines.push("Goals vs actual:".to_string());
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
            lines.push("Goals: none configured".to_string());
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
    ctx: &NutritionCoachContext,
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
pub async fn append_coach_tip(llm: &ChotuLlm, ctx: &NutritionCoachContext, report: &mut String) {
    if !ctx.has_health_data() {
        return;
    }
    match generate_nutrition_coach_tip(llm, ctx).await {
        Ok(tip) => {
            report.push_str("\n• *Coach:* ");
            report.push_str(&tip);
            if !tip.ends_with('\n') {
                report.push('\n');
            }
        }
        Err(e) => {
            eprintln!(
                "Nutrition coach tip failed for {}: {:?}",
                ctx.member_name, e
            );
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
    use chotu_common::NutritionGoals;

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
        s.sleep_hours = None;
        s.perceived_energy = None;
        let ctx = NutritionCoachContext::from_day_summary("Praj", &s, None);
        assert!(!ctx.has_health_data());
    }

    #[test]
    fn has_health_data_true_with_calories() {
        let ctx = NutritionCoachContext::from_day_summary("Praj", &sample_summary(), None);
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
        let ctx = NutritionCoachContext::from_day_summary("Praj", &sample_summary(), Some(&goals));
        let prompt = ctx.to_user_prompt();
        assert!(prompt.contains("Member: Praj"));
        assert!(prompt.contains("today (2026-08-04)"));
        assert!(prompt.contains("Calories: 1800/2000kcal (90%)"));
        assert!(prompt.contains("Protein: 90/150g (60%)"));
        assert!(prompt.contains("Steps: 7500/10000 (75%)"));
    }

    #[test]
    fn trend_context_includes_arrows() {
        let ctx = NutritionCoachContext::from_trend_averages(
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
        assert!(prompt.contains("Goals: none configured"));
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
