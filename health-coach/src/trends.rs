use anyhow::{Context, Result};
use chotu_common::{AppConfig, ChotuLlm, HealthFamilySummary};
use sqlx::SqlitePool;

use crate::coaching::{append_coach_tip, NutritionCoachContext};

/// Builds one Telegram Markdown report per family member covering the last `days` of nutrition/activity.
/// When `llm` is provided, appends a short local-Ollama coach tip for members with logged data.
pub async fn build_nutrition_trend_reports(
    pool: &SqlitePool,
    config: &AppConfig,
    days: i64,
    llm: Option<&ChotuLlm>,
) -> Result<Vec<String>> {
    let days = days.clamp(2, 90);
    let start_date = (chrono::Local::now() - chrono::Duration::days(days - 1))
        .format("%Y-%m-%d")
        .to_string();

    let rows = sqlx::query_as::<_, HealthFamilySummary>(
        r#"
        SELECT *
        FROM health_family_summary
        WHERE date >= ?
        ORDER BY date ASC
        "#,
    )
    .bind(&start_date)
    .fetch_all(pool)
    .await
    .context("Failed to query health_family_summary for trends")?;

    let mut reports = Vec::new();

    for member in &config.family.members {
        let member_rows: Vec<&HealthFamilySummary> = rows
            .iter()
            .filter(|r| r.family_member_id == member.id)
            .collect();

        if member_rows.is_empty() {
            reports.push(format!(
                "📈 *Nutrition Trends: {}* (last {} days)\n\n_No health summaries logged in this window._",
                member.name, days
            ));
            continue;
        }

        let n = member_rows.len() as f64;
        let avg_cal: f64 = member_rows
            .iter()
            .map(|r| r.total_calories_ingested as f64)
            .sum::<f64>()
            / n;
        let avg_protein: f64 = member_rows.iter().map(|r| r.protein_grams).sum::<f64>() / n;
        let avg_carbs: f64 = member_rows.iter().map(|r| r.carbs_grams).sum::<f64>() / n;
        let avg_fats: f64 = member_rows.iter().map(|r| r.fats_grams).sum::<f64>() / n;
        let avg_fiber: f64 = member_rows.iter().map(|r| r.fiber_g).sum::<f64>() / n;
        let avg_steps: f64 = member_rows.iter().map(|r| r.step_count as f64).sum::<f64>() / n;
        let sleep_vals: Vec<f64> = member_rows
            .iter()
            .filter_map(|r| r.sleep_hours)
            .collect();
        let avg_sleep = if sleep_vals.is_empty() {
            None
        } else {
            Some(sleep_vals.iter().sum::<f64>() / sleep_vals.len() as f64)
        };

        let cal_series: Vec<f64> = member_rows
            .iter()
            .map(|r| r.total_calories_ingested as f64)
            .collect();
        let protein_series: Vec<f64> = member_rows.iter().map(|r| r.protein_grams).collect();
        let steps_series: Vec<f64> = member_rows.iter().map(|r| r.step_count as f64).collect();

        let cal_trend = trend_arrow(&cal_series);
        let protein_trend = trend_arrow(&protein_series);
        let steps_trend = trend_arrow(&steps_series);

        let mut msg = format!(
            "📈 *Nutrition Trends: {}* (last {} days, {} logged)\n\n",
            member.name,
            days,
            member_rows.len()
        );
        msg.push_str("• *Averages:*\n");
        msg.push_str(&format!(
            "  - Calories: {:.0} kcal/day {}\n",
            avg_cal, cal_trend
        ));
        msg.push_str(&format!(
            "  - Protein: {:.1}g {}\n",
            avg_protein, protein_trend
        ));
        msg.push_str(&format!("  - Carbs: {:.1}g | Fat: {:.1}g\n", avg_carbs, avg_fats));
        msg.push_str(&format!(
            "  - Steps: {:.0}/day {}\n",
            avg_steps, steps_trend
        ));
        if let Some(sleep) = avg_sleep {
            msg.push_str(&format!("  - Sleep: {:.1} hours/night\n", sleep));
        }

        let goals = member.nutrition_goals.as_ref();
        if let Some(goals) = goals {
            if let Some(progress) = goals.progress_markdown(
                avg_cal.round() as i32,
                avg_protein,
                avg_carbs,
                avg_fats,
                avg_fiber,
                avg_steps.round() as i32,
            ) {
                msg.push('\n');
                msg.push_str(&progress.replace("*Goals:*", "*Avg vs goals:*"));
            }
        }

        msg.push_str("\n• *Daily calories:*\n```\n");
        for row in &member_rows {
            let bar = spark_bar(row.total_calories_ingested as f64, &cal_series);
            msg.push_str(&format!(
                "{} | {:>4} kcal {}\n",
                row.date, row.total_calories_ingested, bar
            ));
        }
        msg.push_str("```\n");

        msg.push_str("\n• *Daily protein:*\n```\n");
        for row in &member_rows {
            msg.push_str(&format!(
                "{} | {:>5.1}g protein | {:>5} steps\n",
                row.date, row.protein_grams, row.step_count
            ));
        }
        msg.push_str("```\n");

        if let Some(llm) = llm {
            let ctx = NutritionCoachContext::from_trend_averages(
                &member.name,
                days,
                member_rows.len(),
                avg_cal,
                avg_protein,
                avg_carbs,
                avg_fats,
                avg_fiber,
                avg_steps,
                avg_sleep,
                goals,
                cal_trend,
                protein_trend,
                steps_trend,
            );
            append_coach_tip(llm, &ctx, &mut msg).await;
        }

        reports.push(msg);
    }

    Ok(reports)
}

/// Compare first-half average vs second-half average of a series.
fn trend_arrow(series: &[f64]) -> &'static str {
    if series.len() < 2 {
        return "→";
    }
    let mid = series.len() / 2;
    let first: f64 = series[..mid].iter().sum::<f64>() / mid as f64;
    let second_slice = &series[mid..];
    let second: f64 = second_slice.iter().sum::<f64>() / second_slice.len() as f64;
    let delta = second - first;
    let threshold = first.abs() * 0.05; // 5% move counts as a trend
    if delta > threshold {
        "↑"
    } else if delta < -threshold {
        "↓"
    } else {
        "→"
    }
}

fn spark_bar(value: f64, series: &[f64]) -> String {
    let max = series.iter().cloned().fold(0.0_f64, f64::max).max(1.0);
    let width = ((value / max) * 10.0).round() as usize;
    "█".repeat(width.min(10))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trend_arrow_up() {
        assert_eq!(trend_arrow(&[100.0, 100.0, 200.0, 200.0]), "↑");
    }

    #[test]
    fn test_trend_arrow_down() {
        assert_eq!(trend_arrow(&[200.0, 200.0, 100.0, 100.0]), "↓");
    }

    #[test]
    fn test_trend_arrow_flat() {
        assert_eq!(trend_arrow(&[100.0, 102.0, 101.0, 100.0]), "→");
    }
}
