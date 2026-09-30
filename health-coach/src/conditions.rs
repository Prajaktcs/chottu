//! Member-authored condition context and successful-delivery food flag dedupe.

use anyhow::Result;
use chotu_common::{AppConfig, FamilyMember};
use chrono::{Duration, NaiveDate, TimeZone};
use sqlx::SqlitePool;

#[derive(Debug, Clone, PartialEq)]
pub struct ConditionCoachContext {
    pub label: String,
    pub watchlist: Vec<String>,
    pub scores: Vec<(String, i32)>,
    pub food_hits: Vec<String>,
    pub as_of: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConditionFoodFlag {
    pub label: String,
    pub tags: Vec<String>,
}

impl ConditionFoodFlag {
    pub fn confirmation_line(&self) -> String {
        format!(
            "\n⚠️ On your {} watchlist: {}",
            self.label,
            self.tags.join(", ")
        )
    }
}

fn private_member<'a>(
    config: &'a AppConfig,
    recipient: Option<&str>,
    member_id: &str,
) -> Option<&'a FamilyMember> {
    let recipient = recipient.filter(|id| id.eq_ignore_ascii_case(member_id))?;
    config
        .family
        .members
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(recipient))
}

/// Household and cross-member requests return before querying private rows.
pub async fn pending_food_flags(
    pool: &SqlitePool,
    config: &AppConfig,
    recipient: Option<&str>,
    member_id: &str,
    date: &str,
    log_id: &str,
) -> Result<Vec<ConditionFoodFlag>> {
    let Some(member) = private_member(config, recipient, member_id) else {
        return Ok(Vec::new());
    };
    if member.health_conditions.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT w.condition_id, w.tag FROM condition_watchlist w JOIN food_log_tags t ON t.tag = w.tag JOIN food_log f ON f.id = t.food_log_id WHERE w.family_member_id = ? AND f.family_member_id = w.family_member_id AND f.id = ? AND NOT EXISTS (SELECT 1 FROM condition_food_flags sent WHERE sent.family_member_id = w.family_member_id AND sent.date = ? AND sent.tag = w.tag) ORDER BY w.condition_id, w.tag",
    ).bind(&member.id).bind(log_id).bind(date).fetch_all(pool).await?;
    Ok(member
        .health_conditions
        .iter()
        .filter_map(|condition| {
            let tags: Vec<_> = rows
                .iter()
                .filter(|(id, _)| id == &condition.id)
                .map(|(_, tag)| tag.clone())
                .collect();
            (!tags.is_empty()).then(|| ConditionFoodFlag {
                label: condition.label.clone(),
                tags,
            })
        })
        .collect())
}

/// Call only after the Signal food confirmation succeeds. Missing delivery is retryable.
pub async fn mark_food_flags_sent(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
    flags: &[ConditionFoodFlag],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    for flag in flags {
        for tag in &flag.tags {
            sqlx::query("INSERT INTO condition_food_flags (family_member_id, date, tag) VALUES (?, ?, ?) ON CONFLICT(family_member_id, date, tag) DO NOTHING")
                .bind(member_id).bind(date).bind(tag).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Snapshot of the member's own watchlists and reported scores; no associations.
pub async fn load_condition_context(
    pool: &SqlitePool,
    config: &AppConfig,
    recipient: Option<&str>,
    member_id: &str,
    as_of: &str,
) -> Result<Vec<ConditionCoachContext>> {
    let Some(member) = private_member(config, recipient, member_id) else {
        return Ok(Vec::new());
    };
    if member.health_conditions.is_empty() {
        return Ok(Vec::new());
    }
    let day = NaiveDate::parse_from_str(as_of, "%Y-%m-%d")?;
    let start = (day - Duration::days(6)).format("%Y-%m-%d").to_string();
    let watch: Vec<(String, String)> = sqlx::query_as(
        "SELECT condition_id, tag FROM condition_watchlist WHERE family_member_id = ? ORDER BY tag",
    )
    .bind(&member.id)
    .fetch_all(pool)
    .await?;
    let scores: Vec<(String, String, i32)> = sqlx::query_as("SELECT condition_id, date, score FROM condition_checkin WHERE family_member_id = ? AND date BETWEEN ? AND ? AND score BETWEEN 0 AND 5 ORDER BY date")
        .bind(&member.id).bind(&start).bind(as_of).fetch_all(pool).await?;
    let tz = config.resolved_tz();
    let midnight = |date: NaiveDate| -> Result<chrono::DateTime<chrono::Utc>> {
        let local = date
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow::anyhow!("Invalid condition day"))?;
        // Midnight can fall in a gap; a skipped civil date has an empty range.
        for second in 0..=86_400 {
            if let Some(dt) = tz
                .from_local_datetime(&(local + Duration::seconds(second)))
                .earliest()
            {
                return Ok(dt.with_timezone(&chrono::Utc));
            }
        }
        Err(anyhow::anyhow!("No valid boundary for condition day"))
    };
    let hits: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT w.condition_id, w.tag FROM condition_watchlist w JOIN food_log_tags t ON t.tag = w.tag JOIN food_log f ON f.id = t.food_log_id WHERE w.family_member_id = ? AND f.family_member_id = w.family_member_id AND f.timestamp >= ? AND f.timestamp < ? ORDER BY w.tag",
    ).bind(&member.id).bind(midnight(day)?).bind(midnight(day + Duration::days(1))?).fetch_all(pool).await?;
    Ok(member
        .health_conditions
        .iter()
        .map(|condition| ConditionCoachContext {
            label: condition.label.clone(),
            watchlist: watch
                .iter()
                .filter(|(id, _)| id == &condition.id)
                .map(|(_, tag)| tag.clone())
                .collect(),
            scores: scores
                .iter()
                .filter(|(id, _, _)| id == &condition.id)
                .map(|(_, date, score)| (date.clone(), *score))
                .collect(),
            food_hits: hits
                .iter()
                .filter(|(id, _)| id == &condition.id)
                .map(|(_, tag)| tag.clone())
                .collect(),
            as_of: as_of.into(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chotu_common::HealthCondition;

    fn config() -> AppConfig {
        let mut config = AppConfig {
            timezone: Some("America/Toronto".into()),
            ..AppConfig::default()
        };
        config.family.members[0].health_conditions = vec![HealthCondition {
            id: "skin".into(),
            label: "Skin symptoms".into(),
            check_in: true,
            lag_window: [1, 3],
            notes: None,
        }];
        let mut other = config.family.members[0].clone();
        other.id = "jordan".into();
        other.health_conditions[0].label = "Jordan private condition".into();
        config.family.members.push(other);
        config
    }

    async fn pool() -> SqlitePool {
        // Use the production upgrade path, which handles legacy schema repairs.
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        sqlx::raw_sql("INSERT INTO condition_watchlist (family_member_id, condition_id, tag) VALUES ('alex', 'skin', 'dairy'), ('alex', 'skin', 'alcohol'), ('jordan', 'skin', 'dairy');")
            .execute(&pool).await.unwrap();
        pool
    }

    async fn food(pool: &SqlitePool, id: &str, member: &str, timestamp: &str, tag: &str) {
        let timestamp = chrono::DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .with_timezone(&chrono::Utc);
        sqlx::query("INSERT INTO food_log (id, family_member_id, timestamp, raw_text_description, estimated_calories) VALUES (?, ?, ?, 'test meal', 100)")
            .bind(id).bind(member).bind(timestamp).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO food_log_tags (food_log_id, tag) VALUES (?, ?)")
            .bind(id)
            .bind(tag)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn flags_wait_for_successful_delivery_and_dedupe_by_member_day_tag() {
        let pool = pool().await;
        let mut config = config();
        let mut second = config.family.members[0].health_conditions[0].clone();
        second.id = "second".into();
        second.label = "Another condition".into();
        config.family.members[0].health_conditions.push(second);
        sqlx::raw_sql("INSERT INTO condition_watchlist (family_member_id, condition_id, tag) VALUES ('alex', 'second', 'dairy')").execute(&pool).await.unwrap();
        food(&pool, "meal", "alex", "2026-09-29T20:00:00Z", "dairy").await;
        food(&pool, "other", "jordan", "2026-09-29T20:00:00Z", "dairy").await;
        let flags = pending_food_flags(&pool, &config, Some("alex"), "alex", "2026-09-29", "meal")
            .await
            .unwrap();
        assert_eq!(
            flags[0].confirmation_line(),
            "\n⚠️ On your Skin symptoms watchlist: dairy"
        );
        assert_eq!(flags.len(), 2);
        // A failed/omitted send does not consume the flag.
        assert_eq!(
            pending_food_flags(&pool, &config, Some("alex"), "alex", "2026-09-29", "meal")
                .await
                .unwrap(),
            flags
        );
        mark_food_flags_sent(&pool, "alex", "2026-09-29", &flags)
            .await
            .unwrap();
        mark_food_flags_sent(&pool, "alex", "2026-09-29", &flags)
            .await
            .unwrap();
        assert!(
            pending_food_flags(&pool, &config, Some("alex"), "alex", "2026-09-29", "meal")
                .await
                .unwrap()
                .is_empty()
        );
        food(&pool, "second", "alex", "2026-09-29T21:00:00Z", "dairy").await;
        assert!(
            pending_food_flags(&pool, &config, Some("alex"), "alex", "2026-09-29", "second")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!pending_food_flags(
            &pool,
            &config,
            Some("jordan"),
            "jordan",
            "2026-09-29",
            "other"
        )
        .await
        .unwrap()
        .is_empty());
        assert!(
            !pending_food_flags(&pool, &config, Some("alex"), "alex", "2026-09-30", "meal")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            pending_food_flags(&pool, &config, Some("alex"), "alex", "2026-09-29", "other")
                .await
                .unwrap()
                .is_empty()
        );
        // Reapplying the additive migration preserves existing delivery state and meals.
        sqlx::raw_sql(include_str!(
            "../../chotu-common/migrations/20260929000000_condition_food_flags.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM condition_food_flags")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn enrichment_adds_conditions_only_for_the_private_recipient() {
        let pool = pool().await;
        let config = config();
        sqlx::raw_sql("INSERT INTO condition_checkin (family_member_id, condition_id, date, score) VALUES ('alex', 'skin', '2026-09-29', 0)").execute(&pool).await.unwrap();
        let ctx = crate::FitnessCoachContext::from_trend_averages(
            "Alex", 7, 0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, None, None, "→", "→", "→",
        );
        assert!(!ctx.has_health_data());
        let private = crate::enrich_coach_context(
            &pool,
            &config,
            "alex",
            ctx,
            crate::CoachEnrichOpts::for_day("2026-09-29").with_private_member(Some("alex")),
        )
        .await;
        assert!(private.has_health_data());
        assert!(private.to_user_prompt().contains("Skin symptoms"));
        assert!(private.to_user_prompt().contains("2026-09-29: 0/5"));
        let household = crate::enrich_coach_context(
            &pool,
            &config,
            "alex",
            private,
            crate::CoachEnrichOpts::for_day("2026-09-29"),
        )
        .await;
        assert!(household.conditions.is_empty());
        assert!(!household.to_user_prompt().contains("Skin symptoms"));
    }

    #[tokio::test]
    async fn household_and_cross_member_context_never_query_private_tables() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        for recipient in [None, Some("jordan")] {
            assert!(
                pending_food_flags(&pool, &config(), recipient, "alex", "2026-09-29", "meal")
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                load_condition_context(&pool, &config(), recipient, "alex", "2026-09-29")
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn coach_context_keeps_sparse_scores_and_uses_dst_civil_day() {
        let pool = pool().await;
        sqlx::raw_sql("INSERT INTO condition_checkin (family_member_id, condition_id, date, score) VALUES ('alex', 'skin', '2026-10-25', 4), ('alex', 'skin', '2026-10-26', 0), ('alex', 'skin', '2026-11-01', 5), ('alex', 'skin', '2026-11-02', 2), ('jordan', 'skin', '2026-11-01', 3);")
            .execute(&pool).await.unwrap();
        // Toronto's November 1 is 25 hours: 04:00 UTC through next day's 05:00 UTC.
        food(&pool, "before", "alex", "2026-11-01T03:59:59Z", "dairy").await;
        food(&pool, "late", "alex", "2026-11-02T04:30:00Z", "alcohol").await;
        food(&pool, "after", "alex", "2026-11-02T05:00:00Z", "dairy").await;
        food(&pool, "other", "jordan", "2026-11-01T12:00:00Z", "dairy").await;
        let contexts = load_condition_context(&pool, &config(), Some("alex"), "alex", "2026-11-01")
            .await
            .unwrap();
        assert_eq!(contexts.len(), 1);
        assert_eq!(
            contexts[0].scores,
            vec![("2026-10-26".into(), 0), ("2026-11-01".into(), 5)]
        );
        assert_eq!(contexts[0].watchlist, vec!["alcohol", "dairy"]);
        assert_eq!(contexts[0].food_hits, vec!["alcohol"]);
        assert_eq!(contexts[0].label, "Skin symptoms");
    }

    #[tokio::test]
    async fn midnight_gap_keeps_condition_scores_and_correct_food_window() {
        let pool = pool().await;
        let mut config = config();
        config.timezone = Some("America/Santiago".into());
        sqlx::raw_sql("INSERT INTO condition_checkin (family_member_id, condition_id, date, score) VALUES ('alex', 'skin', '2026-09-06', 2)").execute(&pool).await.unwrap();
        food(&pool, "before", "alex", "2026-09-06T03:59:59Z", "dairy").await;
        food(&pool, "first", "alex", "2026-09-06T04:00:00Z", "alcohol").await;
        food(&pool, "after", "alex", "2026-09-07T03:00:00Z", "dairy").await;
        let contexts = load_condition_context(&pool, &config, Some("alex"), "alex", "2026-09-06")
            .await
            .unwrap();
        assert_eq!(contexts[0].scores, vec![("2026-09-06".into(), 2)]);
        assert_eq!(contexts[0].food_hits, vec!["alcohol"]);
    }
}
