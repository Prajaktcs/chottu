//! Closed food-tag vocabulary, keyword fallback, and `food_log_tags` persistence.
//! See `docs/condition-tracking-spec.md` (M2).

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use std::collections::HashSet;

/// Seeded vocabulary (`food_tags.tag`). Unknown LLM labels are dropped.
pub const FOOD_TAG_VOCABULARY: &[&str] = &[
    "alcohol",
    "added_sugar",
    "dairy",
    "gluten",
    "red_meat",
    "processed_meat",
    "fried",
    "spicy",
    "nightshades",
    "caffeine",
    "shellfish",
    "eggs",
    "soy",
    "citrus",
];

/// Phrase → vocabulary tag. Longer phrases are matched first.
const KEYWORD_ALIASES: &[(&str, &str)] = &[
    ("soy sauce", "soy"),
    ("energy drink", "caffeine"),
    ("red bull", "caffeine"),
    ("ice cream", "dairy"),
    ("ice cream", "added_sugar"),
    ("hot dog", "processed_meat"),
    ("hotdog", "processed_meat"),
    ("bell pepper", "nightshades"),
    ("french fries", "fried"),
    ("french fry", "fried"),
    ("nachos", "fried"),
    ("nachos", "nightshades"),
    ("onion ring", "fried"),
    ("chicken nugget", "fried"),
    ("tiramisu", "alcohol"),
    ("prosecco", "alcohol"),
    ("champagne", "alcohol"),
    ("cocktail", "alcohol"),
    ("margarita", "alcohol"),
    ("tequila", "alcohol"),
    ("whiskey", "alcohol"),
    ("whisky", "alcohol"),
    ("vodka", "alcohol"),
    ("beers", "alcohol"),
    ("beer", "alcohol"),
    ("wines", "alcohol"),
    ("wine", "alcohol"),
    ("rum", "alcohol"),
    ("gin", "alcohol"),
    ("ipa", "alcohol"),
    ("stout", "alcohol"),
    ("lager", "alcohol"),
    ("soda", "added_sugar"),
    ("coke", "added_sugar"),
    ("pepsi", "added_sugar"),
    ("sprite", "added_sugar"),
    ("dessert", "added_sugar"),
    ("cake", "added_sugar"),
    ("cookie", "added_sugar"),
    ("brownie", "added_sugar"),
    ("candy", "added_sugar"),
    ("donut", "added_sugar"),
    ("doughnut", "added_sugar"),
    ("latte", "dairy"),
    ("latte", "caffeine"),
    ("cappuccino", "dairy"),
    ("cappuccino", "caffeine"),
    ("macchiato", "dairy"),
    ("macchiato", "caffeine"),
    ("yogurt", "dairy"),
    ("yoghurt", "dairy"),
    ("cheese", "dairy"),
    ("butter", "dairy"),
    ("cream", "dairy"),
    ("milk", "dairy"),
    ("mozzarella", "dairy"),
    ("cheddar", "dairy"),
    ("espresso", "caffeine"),
    ("americano", "caffeine"),
    ("coffee", "caffeine"),
    ("matcha", "caffeine"),
    ("pasta", "gluten"),
    ("pizza", "gluten"),
    ("bread", "gluten"),
    ("bagel", "gluten"),
    ("wheat", "gluten"),
    ("croissant", "gluten"),
    ("pretzel", "gluten"),
    ("burger", "red_meat"),
    ("hamburger", "red_meat"),
    ("steak", "red_meat"),
    ("beef", "red_meat"),
    ("lamb", "red_meat"),
    ("pork", "red_meat"),
    ("bacon", "processed_meat"),
    ("sausage", "processed_meat"),
    ("pepperoni", "processed_meat"),
    ("salami", "processed_meat"),
    ("pastrami", "processed_meat"),
    ("ham", "processed_meat"),
    ("fried", "fried"),
    ("fries", "fried"),
    ("tempura", "fried"),
    ("katsu", "fried"),
    ("spicy", "spicy"),
    ("sriracha", "spicy"),
    ("jalapeno", "spicy"),
    ("jalapeño", "spicy"),
    ("chilli", "spicy"),
    ("chili", "spicy"),
    ("eggplant", "nightshades"),
    ("aubergine", "nightshades"),
    ("tomato", "nightshades"),
    ("potato", "nightshades"),
    ("paprika", "nightshades"),
    ("salsa", "nightshades"),
    ("shrimp", "shellfish"),
    ("prawn", "shellfish"),
    ("lobster", "shellfish"),
    ("mussel", "shellfish"),
    ("oyster", "shellfish"),
    ("scallop", "shellfish"),
    ("crab", "shellfish"),
    ("omelette", "eggs"),
    ("omelet", "eggs"),
    ("frittata", "eggs"),
    ("eggs", "eggs"),
    ("egg", "eggs"),
    ("edamame", "soy"),
    ("tempeh", "soy"),
    ("tofu", "soy"),
    ("miso", "soy"),
    ("soy", "soy"),
    ("grapefruit", "citrus"),
    ("clementine", "citrus"),
    ("orange", "citrus"),
    ("lemon", "citrus"),
    ("lime", "citrus"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignedFoodTags {
    pub tags: Vec<String>,
    /// `llm` when the model returned at least one valid tag; otherwise `keyword`.
    pub source: &'static str,
}

pub fn food_tag_classifier_instruction() -> String {
    format!(
        "Also pick zero or more tags from this closed list only (unknown tags are dropped): {}. \
         Use the JSON field `tags` as an array of those slugs. Do not invent tags.",
        FOOD_TAG_VOCABULARY.join(", ")
    )
}

pub fn is_known_food_tag(tag: &str) -> bool {
    FOOD_TAG_VOCABULARY
        .iter()
        .any(|known| known.eq_ignore_ascii_case(tag.trim()))
}

/// Keep vocabulary slugs only, de-duplicated, in vocabulary order.
pub fn sanitize_food_tags(tags: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<String> {
    let mut wanted: HashSet<String> = HashSet::new();
    for tag in tags {
        let normalized = tag
            .as_ref()
            .trim()
            .to_ascii_lowercase()
            .replace('-', "_")
            .replace(' ', "_");
        if is_known_food_tag(&normalized) {
            wanted.insert(normalized);
        }
    }
    FOOD_TAG_VOCABULARY
        .iter()
        .filter(|t| wanted.contains(**t))
        .map(|t| (*t).to_string())
        .collect()
}

pub fn keyword_tags_for(description: &str) -> Vec<String> {
    let hay = padded_tokens(description);
    let mut wanted: HashSet<&str> = HashSet::new();
    let mut aliases: Vec<_> = KEYWORD_ALIASES.iter().collect();
    aliases.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    for (needle, tag) in aliases {
        let n = padded_tokens(needle);
        if hay.contains(&n) {
            wanted.insert(*tag);
        }
    }
    FOOD_TAG_VOCABULARY
        .iter()
        .filter(|t| wanted.contains(*t))
        .map(|t| (*t).to_string())
        .collect()
}

/// Prefer sanitized LLM tags; fall back to the keyword map when the model returned none.
pub fn assign_food_tags(
    llm_tags: impl IntoIterator<Item = impl AsRef<str>>,
    description: &str,
) -> AssignedFoodTags {
    let from_llm = sanitize_food_tags(llm_tags);
    if !from_llm.is_empty() {
        return AssignedFoodTags {
            tags: from_llm,
            source: "llm",
        };
    }
    AssignedFoodTags {
        tags: keyword_tags_for(description),
        source: "keyword",
    }
}

fn padded_tokens(s: &str) -> String {
    let mut out = String::from(" ");
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '+' {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(' ');
        }
    }
    out.push(' ');
    collapse_spaces(&out)
}

fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c == ' ' {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            prev_space = false;
            out.push(c);
        }
    }
    out
}

pub async fn insert_food_log_tags(
    pool: &SqlitePool,
    food_log_id: &str,
    assigned: &AssignedFoodTags,
) -> Result<()> {
    for tag in &assigned.tags {
        sqlx::query(
            "INSERT OR IGNORE INTO food_log_tags (food_log_id, tag, source) VALUES (?, ?, ?)",
        )
        .bind(food_log_id)
        .bind(tag)
        .bind(assigned.source)
        .execute(pool)
        .await
        .with_context(|| format!("insert food_log_tags {food_log_id}/{tag}"))?;
    }
    Ok(())
}

pub async fn delete_food_log_tags(pool: &SqlitePool, food_log_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM food_log_tags WHERE food_log_id = ?")
        .bind(food_log_id)
        .execute(pool)
        .await
        .context("delete food_log_tags by id")?;
    Ok(())
}

pub async fn delete_food_log_tags_for_member_day(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
) -> Result<()> {
    sqlx::query(
        "DELETE FROM food_log_tags WHERE food_log_id IN (\
            SELECT id FROM food_log \
            WHERE family_member_id = ? AND date(timestamp, 'localtime') = ?\
         )",
    )
    .bind(member_id)
    .bind(date)
    .execute(pool)
    .await
    .context("delete food_log_tags for member/day")?;
    Ok(())
}

/// Keyword-tag historical `food_log` rows that have no `food_log_tags` yet.
pub async fn backfill_food_log_keyword_tags(pool: &SqlitePool) -> Result<u64> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT fl.id, fl.raw_text_description FROM food_log fl \
         WHERE NOT EXISTS (SELECT 1 FROM food_log_tags t WHERE t.food_log_id = fl.id)",
    )
    .fetch_all(pool)
    .await
    .context("select untagged food_log rows")?;

    let mut tagged = 0u64;
    for (id, description) in rows {
        let assigned = AssignedFoodTags {
            tags: keyword_tags_for(&description),
            source: "keyword",
        };
        if assigned.tags.is_empty() {
            continue;
        }
        insert_food_log_tags(pool, &id, &assigned).await?;
        tagged += 1;
    }
    Ok(tagged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_drops_unknown_and_orders_by_vocab() {
        assert_eq!(
            sanitize_food_tags(["Fried", "not-a-tag", "alcohol", "FRIED"]),
            vec!["alcohol".to_string(), "fried".to_string()]
        );
    }

    #[test]
    fn keyword_beer_and_nachos() {
        let tags = keyword_tags_for("2 beers and nachos");
        assert!(tags.contains(&"alcohol".to_string()));
        assert!(tags.contains(&"fried".to_string()));
        assert!(tags.contains(&"nightshades".to_string()));
    }

    #[test]
    fn keyword_latte_is_dairy_and_caffeine() {
        let tags = keyword_tags_for("grande latte");
        assert_eq!(
            tags,
            vec!["dairy".to_string(), "caffeine".to_string()]
        );
    }

    #[test]
    fn assign_prefers_llm_when_present() {
        let assigned = assign_food_tags(["alcohol"], "grande latte");
        assert_eq!(assigned.source, "llm");
        assert_eq!(assigned.tags, vec!["alcohol".to_string()]);
    }

    #[test]
    fn assign_falls_back_to_keywords() {
        let assigned = assign_food_tags(["nope"], "beer");
        assert_eq!(assigned.source, "keyword");
        assert_eq!(assigned.tags, vec!["alcohol".to_string()]);
    }
}
