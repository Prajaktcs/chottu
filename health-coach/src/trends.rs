use anyhow::{Context, Result};
use chotu_common::{AppConfig, ChotuLlm, HealthFamilySummary};
use sqlx::SqlitePool;

use crate::coaching::{append_coach_tip, NutritionCoachContext};

/// Builds one report per family member covering the last `days` of nutrition/activity.
/// When `llm` is provided, appends a short local-Ollama coach tip for members with logged data.
/// When `only_member_id` is set, returns at most that member's report (privacy for linked DMs).
pub async fn build_nutrition_trend_reports(
    pool: &SqlitePool,
    config: &AppConfig,
    days: i64,
    llm: Option<&ChotuLlm>,
    only_member_id: Option<&str>,
) -> Result<Vec<String>> {
    let days = days.clamp(2, 90);
    let today = config.now_in_tz().date_naive();
    let (start_date, end_date) = trend_dates(today, days);

    let rows = sqlx::query_as::<_, HealthFamilySummary>(
        r#"
        SELECT *
        FROM health_family_summary
        WHERE date BETWEEN ? AND ?
        ORDER BY date ASC
        "#,
    )
    .bind(&start_date)
    .bind(&end_date)
    .fetch_all(pool)
    .await
    .context("Failed to query health_family_summary for trends")?;

    let mut reports = Vec::new();

    for member in &config.family.members {
        if let Some(only) = only_member_id {
            if !member.id.eq_ignore_ascii_case(only) {
                continue;
            }
        }
        let member_rows: Vec<&HealthFamilySummary> = rows
            .iter()
            .filter(|r| r.family_member_id == member.id)
            .collect();

        let condition_block =
            crate::condition_trend_block(pool, config, only_member_id, &member.id, today, days)
                .await?;

        if member_rows.is_empty() {
            let mut report = format!(
                "📈 *Nutrition Trends: {}* (last {} days)\n\n_No health summaries logged in this window._",
                member.name, days
            );
            let ctx = NutritionCoachContext::from_trend_averages(
                &member.name,
                days,
                0,
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
                None,
                member.nutrition_goals.as_ref(),
                "→",
                "→",
                "→",
            );
            let ctx = crate::coach_enrich::enrich_coach_context(
                pool,
                config,
                &member.id,
                ctx,
                crate::coach_enrich::CoachEnrichOpts::for_trends(&start_date, &end_date)
                    .with_private_member(only_member_id),
            )
            .await;
            report.push_str(&condition_block);
            if let Some(llm) = llm {
                append_coach_tip(llm, &ctx, &mut report).await;
            }
            reports.push(report);
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
        let sleep_vals: Vec<f64> = member_rows.iter().filter_map(|r| r.sleep_hours).collect();
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
        msg.push_str(&format!(
            "  - Carbs: {:.1}g | Fat: {:.1}g\n",
            avg_carbs, avg_fats
        ));
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

        msg.push_str(&condition_block);

        if let Some(llm) = llm {
            let ctx = NutritionCoachContext::from_trend_summaries(
                &member.name,
                days,
                &member_rows,
                goals,
            );
            let ctx = crate::coach_enrich::enrich_coach_context(
                pool,
                config,
                &member.id,
                ctx,
                crate::coach_enrich::CoachEnrichOpts::for_trends(&start_date, &end_date)
                    .with_private_member(only_member_id),
            )
            .await;
            append_coach_tip(llm, &ctx, &mut msg).await;
        }

        reports.push(msg);
    }

    Ok(reports)
}

/// Compare first-half average vs second-half average of a series.
fn trend_dates(today: chrono::NaiveDate, days: i64) -> (String, String) {
    (
        (today - chrono::Duration::days(days - 1))
            .format("%Y-%m-%d")
            .to_string(),
        today.format("%Y-%m-%d").to_string(),
    )
}

pub(crate) fn trend_arrow(series: &[f64]) -> &'static str {
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
    fn trend_dates_count_civil_days_across_spring_forward() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-03-09T04:30:00Z")
            .unwrap()
            .with_timezone(&chrono_tz::America::Toronto);
        assert_eq!(
            trend_dates(now.date_naive(), 2),
            ("2026-03-08".into(), "2026-03-09".into())
        );
    }

    #[tokio::test]
    async fn symptom_only_trends_are_private_and_not_skipped() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let mut config = AppConfig::default();
        config.family.members[0].health_conditions = vec![chotu_common::HealthCondition {
            id: "skin".into(),
            label: "Skin symptoms".into(),
            check_in: true,
            lag_window: [1, 3],
            notes: None,
        }];
        let date = config.now_in_tz().format("%Y-%m-%d").to_string();
        sqlx::query("INSERT INTO condition_checkin (family_member_id, condition_id, date, score) VALUES ('alex', 'skin', ?, 0)").bind(&date).execute(&pool).await.unwrap();
        let private = build_nutrition_trend_reports(&pool, &config, 7, None, Some("alex"))
            .await
            .unwrap();
        assert!(private[0].contains("Skin symptoms: 0/5"));
        let household = build_nutrition_trend_reports(&pool, &config, 7, None, None)
            .await
            .unwrap();
        assert!(!household[0].contains("Skin symptoms"));
        assert!(!household[0].contains("Reported symptoms"));
    }

    #[tokio::test]
    async fn condition_timeline_covers_requested_window_with_or_without_nutrition() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let mut config = AppConfig::default();
        config.family.members[0].health_conditions = vec![chotu_common::HealthCondition {
            id: "skin".into(),
            label: "Private symptoms".into(),
            check_in: true,
            lag_window: [1, 3],
            notes: None,
        }];
        let today = config.now_in_tz().date_naive();
        // Include scores older than the coach context's seven-day window.
        for offset in 7..14 {
            sqlx::query("INSERT INTO condition_checkin (family_member_id, condition_id, date, score) VALUES ('alex', 'skin', ?, 2)")
                .bind((today - chrono::Duration::days(offset)).to_string()).execute(&pool).await.unwrap();
        }
        for with_nutrition in [false, true] {
            if with_nutrition {
                sqlx::query(
                    "INSERT INTO health_family_summary (family_member_id, date) VALUES ('alex', ?)",
                )
                .bind(today.to_string())
                .execute(&pool)
                .await
                .unwrap();
            }
            let private = build_nutrition_trend_reports(&pool, &config, 14, None, Some("alex"))
                .await
                .unwrap();
            assert!(private[0].contains("Private symptoms* (last 14 days, 7 check-ins)"));
            assert!(private[0].contains("2 2 2 2 2 2 2 . . . . . . ."));
            let household = build_nutrition_trend_reports(&pool, &config, 14, None, None)
                .await
                .unwrap();
            assert!(household
                .iter()
                .all(|report| !report.contains("Private symptoms")));
        }
    }

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
