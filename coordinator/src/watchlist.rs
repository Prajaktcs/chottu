//! Signal command behavior for member-authored condition watchlists (M5).

use anyhow::Result;
use chotu_common::AppConfig;
use sqlx::SqlitePool;

const WATCH_USAGE: &str =
    "Usage: /watch | /watch add <condition_id> <tag> | /watch remove <condition_id> <tag>";

#[derive(sqlx::FromRow)]
struct FoodTag {
    tag: String,
    label: String,
    description: String,
}

async fn food_tags(pool: &SqlitePool) -> Result<Vec<FoodTag>> {
    Ok(
        sqlx::query_as("SELECT tag, label, description FROM food_tags ORDER BY tag")
            .fetch_all(pool)
            .await?,
    )
}

pub async fn tags_reply(pool: &SqlitePool, args: &str) -> Result<String> {
    if !args.trim().is_empty() {
        return Ok("Usage: /tags".into());
    }
    let tags = food_tags(pool).await?;
    let mut reply = "Food tags:\n".to_string();
    for tag in tags {
        reply.push_str(&format!(
            "• {} — {}: {}\n",
            tag.tag, tag.label, tag.description
        ));
    }
    reply.push_str("\nUse /watch in your linked DM to choose tags to track.");
    Ok(reply)
}

/// `member_id` comes only from the authenticated caller scope, never arguments.
/// Deny household requests before reading any condition definitions or rows.
pub async fn watch_reply(
    pool: &SqlitePool,
    config: &AppConfig,
    member_id: Option<&str>,
    args: &str,
) -> Result<String> {
    let Some(member) = member_id.and_then(|id| {
        config
            .family
            .members
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(id))
    }) else {
        return Ok(
            "Use /watch in your linked personal DM to manage your condition watchlists.".into(),
        );
    };
    let conditions: Vec<_> = member
        .health_conditions
        .iter()
        .filter(|c| !c.id.trim().is_empty() && !c.label.trim().is_empty())
        .collect();
    if conditions.is_empty() {
        return Ok("No health conditions configured for you yet. Add health_conditions under your member in private config.yaml, then restart Chotu.".into());
    }

    let tokens: Vec<_> = args.split_whitespace().collect();
    if tokens.is_empty() {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT condition_id, tag FROM condition_watchlist WHERE family_member_id = ? ORDER BY condition_id, tag",
        ).bind(&member.id).fetch_all(pool).await?;
        let mut reply = "Your condition watchlists:\n".to_string();
        for condition in conditions {
            let tags: Vec<_> = rows
                .iter()
                .filter(|(id, _)| id == &condition.id)
                .map(|(_, tag)| tag.as_str())
                .collect();
            reply.push_str(&format!(
                "• {} ({}): {}\n",
                condition.label,
                condition.id,
                if tags.is_empty() {
                    "no tags yet".to_string()
                } else {
                    tags.join(", ")
                }
            ));
        }
        reply.push_str(&format!("\n{WATCH_USAGE}\n/tags lists available tags."));
        return Ok(reply);
    }

    let valid_conditions = conditions
        .iter()
        .map(|c| c.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let usage = format!(
        "{WATCH_USAGE}\nYour conditions: {valid_conditions}\nUse /tags for available tags."
    );
    if tokens.len() != 3 || !matches!(tokens[0].to_ascii_lowercase().as_str(), "add" | "remove") {
        return Ok(usage);
    }
    let Some(condition) = conditions
        .iter()
        .find(|c| c.id.eq_ignore_ascii_case(tokens[1]))
    else {
        return Ok(format!("Unknown condition.\n{usage}"));
    };
    let tags = food_tags(pool).await?;
    let Some(tag) = tags.iter().find(|t| t.tag.eq_ignore_ascii_case(tokens[2])) else {
        let valid_tags = tags
            .iter()
            .map(|t| t.tag.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Ok(format!(
            "Unknown food tag.\n{usage}\nAvailable tags: {valid_tags}"
        ));
    };

    if tokens[0].eq_ignore_ascii_case("add") {
        let changed = sqlx::query("INSERT INTO condition_watchlist (family_member_id, condition_id, tag) VALUES (?, ?, ?) ON CONFLICT(family_member_id, condition_id, tag) DO NOTHING")
            .bind(&member.id).bind(&condition.id).bind(&tag.tag).execute(pool).await?.rows_affected();
        Ok(if changed == 0 {
            format!(
                "{} is already on your {} watchlist.",
                tag.tag, condition.label
            )
        } else {
            format!("Added {} to your {} watchlist.", tag.tag, condition.label)
        })
    } else {
        let changed = sqlx::query("DELETE FROM condition_watchlist WHERE family_member_id = ? AND condition_id = ? AND tag = ?")
            .bind(&member.id).bind(&condition.id).bind(&tag.tag).execute(pool).await?.rows_affected();
        Ok(if changed == 0 {
            format!("{} was not on your {} watchlist.", tag.tag, condition.label)
        } else {
            format!(
                "Removed {} from your {} watchlist.",
                tag.tag, condition.label
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chotu_common::HealthCondition;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::raw_sql(include_str!(
            "../../chotu-common/migrations/20260824000003_condition_tracking.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    fn config() -> AppConfig {
        let mut config = AppConfig::default();
        config.family.members[0].health_conditions = vec![HealthCondition {
            id: "skin".into(),
            label: "Skin symptoms".into(),
            check_in: false,
            lag_window: [1, 3],
            notes: None,
        }];
        let mut other = config.family.members[0].clone();
        other.id = "jordan".into();
        other.health_conditions[0].label = "Jordan private condition".into();
        config.family.members.push(other);
        config
    }

    #[tokio::test]
    async fn tags_show_the_seeded_vocabulary_without_member_data() {
        let pool = pool().await;
        let reply = tags_reply(&pool, "").await.unwrap();
        for tag in chotu_common::food_tags::FOOD_TAG_VOCABULARY {
            assert!(reply.contains(&format!("• {tag} —")), "{tag}");
        }
        assert!(reply.contains("beer, wine"));
        assert_eq!(tags_reply(&pool, "add").await.unwrap(), "Usage: /tags");
    }

    #[tokio::test]
    async fn watchlists_add_remove_idempotently_and_keep_members_isolated() {
        let pool = pool().await;
        let config = config();
        let empty = watch_reply(&pool, &config, Some("alex"), "").await.unwrap();
        assert!(empty.contains("Skin symptoms (skin): no tags yet"));
        assert!(!empty.contains("Jordan"));
        assert!(watch_reply(&pool, &config, Some("alex"), "ADD SKIN DAIRY")
            .await
            .unwrap()
            .starts_with("Added dairy"));
        assert!(watch_reply(&pool, &config, Some("alex"), "add skin dairy")
            .await
            .unwrap()
            .contains("already"));
        watch_reply(&pool, &config, Some("jordan"), "add skin alcohol")
            .await
            .unwrap();
        let alex = watch_reply(&pool, &config, Some("alex"), "").await.unwrap();
        watch_reply(&pool, &config, Some("jordan"), "add skin dairy")
            .await
            .unwrap();
        assert!(alex.contains("skin): dairy"));
        assert!(!alex.contains("alcohol"));
        assert!(
            watch_reply(&pool, &config, Some("alex"), "remove skin dairy")
                .await
                .unwrap()
                .starts_with("Removed")
        );
        assert!(
            watch_reply(&pool, &config, Some("alex"), "remove skin dairy")
                .await
                .unwrap()
                .contains("was not")
        );
        let rows: Vec<(String, String, String)> =
            sqlx::query_as("SELECT family_member_id, condition_id, tag FROM condition_watchlist ORDER BY family_member_id, condition_id, tag")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![
                ("jordan".into(), "skin".into(), "alcohol".into()),
                ("jordan".into(), "skin".into(), "dairy".into())
            ]
        );
    }

    #[tokio::test]
    async fn watchlist_rejects_invalid_requests_without_writes() {
        let pool = pool().await;
        let config = config();
        for args in [
            "add",
            "add skin",
            "add skin dairy extra",
            "clear skin dairy",
            "add jordan dairy",
            "add skin imaginary",
            "add skin ');DROP TABLE food_tags;--",
        ] {
            let reply = watch_reply(&pool, &config, Some("alex"), args)
                .await
                .unwrap();
            assert!(reply.contains("Usage:"), "{args}");
            assert!(reply.contains("Your conditions: skin"));
        }
        let unknown_tag = watch_reply(&pool, &config, Some("alex"), "add skin imaginary")
            .await
            .unwrap();
        assert!(unknown_tag.contains("Available tags:"));
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM condition_watchlist")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0);
    }

    #[tokio::test]
    async fn household_and_unrecognized_member_cannot_read_or_write_conditions() {
        // A database without the tables proves authorization happens before queries.
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        for member in [None, Some("unknown")] {
            for args in ["", "add skin dairy", "remove skin dairy"] {
                let reply = watch_reply(&pool, &config(), member, args).await.unwrap();
                assert!(reply.contains("linked personal DM"));
                assert!(!reply.contains("Skin symptoms"));
                assert!(!reply.contains("Jordan"));
            }
        }
        let reply = watch_reply(&pool, &AppConfig::default(), Some("alex"), "")
            .await
            .unwrap();
        assert!(reply.contains("No health conditions configured"));
    }
}
