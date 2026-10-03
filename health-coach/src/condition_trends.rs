//! Deterministic, private symptom timelines and descriptive lagged comparisons.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use chotu_common::{civil_day_bounds_utc, escape_md, AppConfig};
use chrono::{DateTime, Duration, NaiveDate};
use sqlx::SqlitePool;

#[derive(Debug)]
struct Day {
    date: NaiveDate,
    score: Option<i32>,
    sleep: Option<f64>,
    hits: BTreeSet<String>,
    food_covered: bool,
}

#[derive(Debug)]
struct Trend {
    label: String,
    lag: [i32; 2],
    watch: Vec<String>,
    days: Vec<Day>,
}

/// Check the recipient boundary before touching any condition tables.
async fn load_trends(
    pool: &SqlitePool,
    config: &AppConfig,
    recipient: Option<&str>,
    member_id: &str,
    end: NaiveDate,
    days: i64,
) -> Result<Vec<Trend>> {
    let Some(member) = crate::conditions::private_member(config, recipient, member_id) else {
        return Ok(Vec::new());
    };
    if member.health_conditions.is_empty() {
        return Ok(Vec::new());
    }
    let days = days.clamp(2, 90);
    let start = end - Duration::days(days - 1);
    let first_food_day = start - Duration::days(14);
    let (food_start, _) = civil_day_bounds_utc(&first_food_day.to_string(), config.resolved_tz())?;
    let (_, food_end) = civil_day_bounds_utc(&end.to_string(), config.resolved_tz())?;
    let watch: Vec<(String, String)> = sqlx::query_as(
        "SELECT condition_id, tag FROM condition_watchlist WHERE family_member_id = ? ORDER BY tag",
    )
    .bind(&member.id)
    .fetch_all(pool)
    .await?;
    let scores: Vec<(String, String, i32)> = sqlx::query_as(
        "SELECT condition_id, date, score FROM condition_checkin WHERE family_member_id = ? AND date BETWEEN ? AND ? AND score BETWEEN 0 AND 5",
    ).bind(&member.id).bind(start.to_string()).bind(end.to_string()).fetch_all(pool).await?;
    let sleep: Vec<(String, Option<f64>)> = sqlx::query_as(
        "SELECT date, sleep_hours FROM health_family_summary WHERE family_member_id = ? AND date BETWEEN ? AND ?",
    ).bind(&member.id).bind(start.to_string()).bind(end.to_string()).fetch_all(pool).await?;
    // Normalize SQLite's supported timestamp representations before timezone conversion.
    let food: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT strftime('%Y-%m-%dT%H:%M:%fZ', f.timestamp), t.tag FROM food_log f LEFT JOIN food_log_tags t ON t.food_log_id = f.id WHERE f.family_member_id = ? AND julianday(f.timestamp) >= julianday(?) AND julianday(f.timestamp) < julianday(?)",
    ).bind(&member.id).bind(food_start).bind(food_end).fetch_all(pool).await?;
    let mut food_days: BTreeMap<NaiveDate, BTreeSet<String>> = BTreeMap::new();
    for (timestamp, tag) in food {
        let day = DateTime::parse_from_rfc3339(&timestamp)?
            .with_timezone(&config.resolved_tz())
            .date_naive();
        let tags = food_days.entry(day).or_default();
        if let Some(tag) = tag {
            tags.insert(tag);
        }
    }
    let sleep: BTreeMap<_, _> = sleep.into_iter().collect();
    let mut trends = Vec::new();
    for condition in &member.health_conditions {
        let [min, max] = condition.lag_window;
        // Config validation warns rather than rejecting; don't silently repair bad windows.
        if !(0 <= min && min <= max && max <= 14) {
            continue;
        }
        let watch: Vec<_> = watch
            .iter()
            .filter(|(id, _)| id == &condition.id)
            .map(|(_, tag)| tag.clone())
            .collect();
        let scores: BTreeMap<_, _> = scores
            .iter()
            .filter(|(id, _, _)| id == &condition.id)
            .map(|(_, date, score)| (date.as_str(), *score))
            .collect();
        let mut timeline = Vec::new();
        for offset in 0..days {
            let date = start + Duration::days(offset);
            let mut hits = BTreeSet::new();
            let mut food_covered = true;
            for lag in min..=max {
                match food_days.get(&(date - Duration::days(i64::from(lag)))) {
                    Some(tags) => {
                        hits.extend(watch.iter().filter(|tag| tags.contains(*tag)).cloned())
                    }
                    None => food_covered = false,
                }
            }
            timeline.push(Day {
                date,
                score: scores.get(date.to_string().as_str()).copied(),
                sleep: sleep
                    .get(&date.to_string())
                    .copied()
                    .flatten()
                    .filter(|hours| hours.is_finite() && *hours >= 0.0 && *hours <= 24.0),
                hits,
                food_covered,
            });
        }
        trends.push(Trend {
            label: condition.label.clone(),
            lag: [min, max],
            watch,
            days: timeline,
        });
    }
    Ok(trends)
}

/// Match distinct recorded days by sleep (within half an hour), without reusing controls.
/// Missing scores, sleep, or food-log days are excluded rather than imputed as zero.
fn association(trend: &Trend, tag: &str) -> Option<String> {
    let eligible: Vec<_> = trend
        .days
        .iter()
        .filter(|day| day.food_covered && day.score.is_some() && day.sleep.is_some())
        .collect();
    let exposed: Vec<_> = eligible
        .iter()
        .copied()
        .filter(|day| day.hits.contains(tag))
        .collect();
    let mut controls: Vec<_> = eligible
        .iter()
        .copied()
        .filter(|day| !day.hits.contains(tag))
        .collect();
    let mut differences = Vec::new();
    for day in exposed {
        let best = controls
            .iter()
            .enumerate()
            .map(|(i, control)| (i, (day.sleep.unwrap() - control.sleep.unwrap()).abs()))
            .filter(|(_, delta)| *delta <= 0.5)
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((index, _)) = best {
            let control = controls.remove(index);
            differences.push(f64::from(day.score.unwrap() - control.score.unwrap()));
        }
    }
    if differences.len() < 10 {
        return None;
    }
    let delta = differences.iter().sum::<f64>() / differences.len() as f64;
    Some(format!(
        "Tentative: scores averaged {:.1} points {} with {} logged in the lag window vs no matching tag logged ({} days per group, sleep within 0.5h). Logs may be incomplete; this does not show cause.",
        delta.abs(), if delta >= 0.0 { "higher" } else { "lower" },
        escape_md(tag), differences.len(),
    ))
}

fn render(trends: &[Trend]) -> String {
    let mut out = String::new();
    for trend in trends {
        let count = trend.days.iter().filter(|day| day.score.is_some()).count();
        if count == 0 {
            continue;
        }
        if count < 7 {
            let latest = trend
                .days
                .iter()
                .rev()
                .find(|day| day.score.is_some())
                .unwrap();
            out.push_str(&format!("\n• Reported symptoms — {}: {}/5 ({}); {count}/7 check-ins needed for a timeline.\n", escape_md(&trend.label), latest.score.unwrap(), latest.date));
            continue;
        }
        out.push_str(&format!(
            "\n🩺 *{}* (last {} days, {} check-ins)\n",
            escape_md(&trend.label),
            trend.days.len(),
            count
        ));
        out.push_str(&format!(
            "{} → {} · 0 = calm, 5 = flare, . = skipped\n",
            trend.days[0].date,
            trend.days.last().unwrap().date
        ));
        out.push_str(
            &trend
                .days
                .iter()
                .map(|day| day.score.map_or(".".into(), |score| score.to_string()))
                .collect::<Vec<_>>()
                .join(" "),
        );
        out.push_str(&format!(
            "\nWatchlist matches {}–{} days before each score:\n",
            trend.lag[0], trend.lag[1]
        ));
        if trend.watch.is_empty() {
            out.push_str("No watchlist tags configured.\n");
        } else if !trend
            .days
            .iter()
            .any(|day| day.score.is_some() && !day.hits.is_empty())
        {
            out.push_str(
                "No matching tags logged in these lag windows; missing food logs remain unknown.\n",
            );
        }
        for tag in &trend.watch {
            let dates: Vec<_> = trend
                .days
                .iter()
                .filter(|day| day.score.is_some() && day.hits.contains(tag))
                .map(|day| day.date.format("%b %d").to_string())
                .collect();
            if !dates.is_empty() {
                out.push_str(&format!("• {}: {}\n", escape_md(tag), dates.join(", ")));
            }
        }
        let sleep: Vec<_> = trend
            .days
            .iter()
            .filter(|day| day.score.is_some())
            .filter_map(|day| day.sleep)
            .collect();
        if sleep.is_empty() {
            out.push_str("Sleep on scored days: not recorded.\n");
        } else {
            out.push_str(&format!(
                "Sleep on scored days: {:.1}h average ({} of {} days).\n",
                sleep.iter().sum::<f64>() / sleep.len() as f64,
                sleep.len(),
                count
            ));
        }
        let comparisons: Vec<_> = trend
            .watch
            .iter()
            .filter_map(|tag| association(trend, tag))
            .collect();
        if comparisons.is_empty() && !trend.watch.is_empty() {
            out.push_str("No association estimate: each group needs 10 scored days with food logs across the lag window and comparable recorded sleep.\n");
        } else {
            for comparison in comparisons {
                out.push_str(&format!("• {comparison}\n"));
            }
        }
    }
    out
}

pub async fn condition_trend_block(
    pool: &SqlitePool,
    config: &AppConfig,
    recipient: Option<&str>,
    member_id: &str,
    end: NaiveDate,
    days: i64,
) -> Result<String> {
    Ok(render(
        &load_trends(pool, config, recipient, member_id, end, days).await?,
    ))
}

/// Compact Sunday brief addition, ending on Saturday so today's check-in isn't missing yet.
pub async fn weekly_condition_lines(
    pool: &SqlitePool,
    config: &AppConfig,
    recipient: Option<&str>,
    end: NaiveDate,
) -> Result<String> {
    let Some(member_id) = recipient else {
        return Ok(String::new());
    };
    let trends = load_trends(pool, config, recipient, member_id, end, 7).await?;
    let mut out = String::new();
    for trend in trends {
        let scored: Vec<_> = trend
            .days
            .iter()
            .filter(|day| day.score.is_some())
            .collect();
        if scored.is_empty() {
            continue;
        }
        let average = scored
            .iter()
            .map(|day| f64::from(day.score.unwrap()))
            .sum::<f64>()
            / scored.len() as f64;
        let hits = scored.iter().filter(|day| !day.hits.is_empty()).count();
        let sleep: Vec<_> = scored.iter().filter_map(|day| day.sleep).collect();
        out.push_str(&format!("\n• {} — last 7 days ending {}: {}/7 check-ins, average {:.1}/5; watchlist tags in the {}–{} day lag window on {} scored days", escape_md(&trend.label), end, scored.len(), average, trend.lag[0], trend.lag[1], hits));
        if !sleep.is_empty() {
            out.push_str(&format!(
                "; sleep {:.1}h ({} recorded days)",
                sleep.iter().sum::<f64>() / sleep.len() as f64,
                sleep.len()
            ));
        }
        out.push_str(". Descriptive only.\n");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(lag: [i32; 2]) -> AppConfig {
        let mut config = AppConfig {
            timezone: Some("America/Toronto".into()),
            ..AppConfig::default()
        };
        config.family.members[0].health_conditions = vec![chotu_common::HealthCondition {
            id: "skin".into(),
            label: "Skin symptoms".into(),
            check_in: true,
            lag_window: lag,
            notes: None,
        }];
        config
    }

    async fn pool() -> SqlitePool {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        sqlx::query("INSERT INTO condition_watchlist (family_member_id, condition_id, tag) VALUES ('alex', 'skin', 'dairy')").execute(&pool).await.unwrap();
        pool
    }

    async fn score(pool: &SqlitePool, date: NaiveDate, score: i32, sleep: Option<f64>) {
        sqlx::query("INSERT INTO condition_checkin (family_member_id, condition_id, date, score) VALUES ('alex', 'skin', ?, ?)").bind(date.to_string()).bind(score).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO health_family_summary (family_member_id, date, sleep_hours) VALUES ('alex', ?, ?)").bind(date.to_string()).bind(sleep).execute(pool).await.unwrap();
    }

    async fn meal(pool: &SqlitePool, id: &str, member: &str, timestamp: &str, dairy: bool) {
        sqlx::query("INSERT INTO food_log (id, family_member_id, timestamp, raw_text_description, estimated_calories) VALUES (?, ?, ?, 'meal', 100)")
            .bind(id).bind(member).bind(timestamp).execute(pool).await.unwrap();
        if dairy {
            sqlx::query("INSERT INTO food_log_tags (food_log_id, tag) VALUES (?, 'dairy')")
                .bind(id)
                .execute(pool)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn privacy_boundary_returns_before_any_query() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let end = NaiveDate::from_ymd_opt(2026, 11, 2).unwrap();
        for recipient in [None, Some("jordan")] {
            assert!(
                condition_trend_block(&pool, &config([1, 3]), recipient, "alex", end, 14)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(weekly_condition_lines(&pool, &config([1, 3]), None, end)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn lag_uses_civil_days_across_dst_and_keeps_skipped_days() {
        let pool = pool().await;
        let config = config([1, 1]);
        let end = NaiveDate::from_ymd_opt(2026, 11, 2).unwrap();
        for offset in 0..8 {
            if offset != 3 {
                score(&pool, end - Duration::days(offset), 0, Some(7.5)).await;
            }
        }
        // November 1 in Toronto is 25 hours; all three timestamps are on distinct days.
        meal(&pool, "before", "alex", "2026-11-01T03:59:59Z", true).await;
        meal(&pool, "late", "alex", "2026-11-02T04:59:59Z", true).await;
        meal(&pool, "after", "alex", "2026-11-02T05:00:00Z", true).await;
        meal(&pool, "other", "jordan", "2026-10-31T12:00:00Z", true).await;
        let trends = load_trends(&pool, &config, Some("Alex"), "alex", end, 8)
            .await
            .unwrap();
        assert!(trends[0].days[7].hits.contains("dairy"));
        assert!(trends[0].days[6].hits.contains("dairy"));
        assert!(!trends[0].days[5].hits.contains("dairy"));
        assert_eq!(trends[0].days[4].score, None);
        let rendered = render(&trends);
        assert!(rendered.contains("0 0 0 0 . 0 0 0"));
        assert!(rendered.contains("dairy: Nov 01, Nov 02"));
        assert!(rendered.contains("7.5h average (7 of 7 days)"));
        assert!(rendered.contains("No association estimate"));
        assert!(!rendered.contains("Tentative:"));
        let weekly = weekly_condition_lines(&pool, &config, Some("alex"), end)
            .await
            .unwrap();
        assert!(weekly.contains("6/7 check-ins"));
        assert!(weekly.contains("average 0.0/5"));
    }

    #[tokio::test]
    async fn association_requires_ten_distinct_sleep_matched_days_in_each_arm() {
        let pool = pool().await;
        let config = config([0, 0]);
        let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        for offset in 0..20 {
            let date = start + Duration::days(offset);
            let exposed = offset % 2 == 0;
            score(
                &pool,
                date,
                if exposed { 3 } else { 1 },
                Some(if exposed { 7.4 } else { 7.0 }),
            )
            .await;
            meal(
                &pool,
                &date.to_string(),
                "alex",
                &format!("{date}T12:00:00Z"),
                exposed,
            )
            .await;
        }
        let end = start + Duration::days(19);
        let trends = load_trends(&pool, &config, Some("alex"), "alex", end, 20)
            .await
            .unwrap();
        let comparison = association(&trends[0], "dairy").unwrap();
        assert!(comparison.contains("2.0 points higher"));
        assert!(comparison.contains("10 days per group"));
        assert!(comparison.contains("does not show cause"));
        let short = load_trends(&pool, &config, Some("alex"), "alex", end, 19)
            .await
            .unwrap();
        assert!(association(&short[0], "dairy").is_none());
        // Ten controls with poorer sleep cannot support a sleep-matched comparison.
        sqlx::query("UPDATE health_family_summary SET sleep_hours = 5 WHERE date IN (SELECT date FROM condition_checkin WHERE score = 1)").execute(&pool).await.unwrap();
        let poor_sleep = load_trends(&pool, &config, Some("alex"), "alex", end, 20)
            .await
            .unwrap();
        assert!(association(&poor_sleep[0], "dairy").is_none());
        sqlx::query("UPDATE health_family_summary SET sleep_hours = 0")
            .execute(&pool)
            .await
            .unwrap();
        let zero_sleep = load_trends(&pool, &config, Some("alex"), "alex", end, 20)
            .await
            .unwrap();
        assert!(association(&zero_sleep[0], "dairy").is_some());
        sqlx::query("UPDATE health_family_summary SET sleep_hours = NULL WHERE date = ?")
            .bind(start.to_string())
            .execute(&pool)
            .await
            .unwrap();
        let missing_sleep = load_trends(&pool, &config, Some("alex"), "alex", end, 20)
            .await
            .unwrap();
        assert!(association(&missing_sleep[0], "dairy").is_none());
        sqlx::query("UPDATE health_family_summary SET sleep_hours = 7")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM food_log WHERE id = ?")
            .bind(start.to_string())
            .execute(&pool)
            .await
            .unwrap();
        let missing_food = load_trends(&pool, &config, Some("alex"), "alex", end, 20)
            .await
            .unwrap();
        assert!(association(&missing_food[0], "dairy").is_none());
    }

    #[tokio::test]
    async fn full_lag_window_and_configured_tags_only() {
        let pool = pool().await;
        let end = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        score(&pool, end, 4, None).await;
        meal(
            &pool,
            "two-days-before",
            "alex",
            "2026-09-08T12:00:00Z",
            true,
        )
        .await;
        meal(&pool, "same-day", "alex", "2026-09-10T12:00:00Z", true).await;
        let trends = load_trends(&pool, &config([1, 3]), Some("alex"), "alex", end, 7)
            .await
            .unwrap();
        let day = trends[0].days.last().unwrap();
        assert!(day.hits.contains("dairy"));
        assert!(!day.food_covered);
        assert!(day.sleep.is_none());
        assert!(render(&trends).contains("1/7 check-ins needed"));
        let no_same_day = load_trends(&pool, &config([1, 1]), Some("alex"), "alex", end, 7)
            .await
            .unwrap();
        assert!(no_same_day[0].days.last().unwrap().hits.is_empty());
        let invalid = load_trends(&pool, &config([3, 1]), Some("alex"), "alex", end, 7)
            .await
            .unwrap();
        assert!(invalid.is_empty());
    }
}
