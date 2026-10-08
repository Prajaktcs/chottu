//! Closed food-tag vocabulary, keyword fallback, and `food_log_tags` persistence.
//! See `docs/condition-tracking-spec.md` (M2).

use anyhow::{Context, Result};
use sqlx::{Sqlite, SqlitePool, Transaction};
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
    ("paneer", "dairy"),
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

fn exclusion_scope(clause: &[&str], start: usize, words: usize) -> (bool, bool) {
    let mut before = start;
    while before > 0
        && matches!(
            clause[before - 1],
            "any" | "added" | "scrambled" | "boiled" | "fried" | "poached"
        )
    {
        before -= 1;
    }
    let negated = before > 0 && matches!(clause[before - 1], "no" | "without" | "not")
        || clause.get(start + words) == Some(&"free");
    // A trailing "not eggs" belongs to the preceding named component, even
    // when punctuation split it off. Standalone no/without facts remain global.
    let whole_meal = (clause.len() == words + 1 && clause.first() != Some(&"not"))
        || clause
            .windows(3)
            .any(|words| words == ["in", "this", "meal"])
        || clause
            .windows(3)
            .any(|words| words == ["in", "the", "meal"])
        || clause.contains(&"anywhere");
    (negated, whole_meal)
}

fn whole_meal_qualifier_start(clause: &[&str]) -> Option<usize> {
    clause.iter().enumerate().find_map(|(index, word)| {
        (*word == "anywhere"
            || (*word == "in"
                && matches!(clause.get(index + 1), Some(&"this" | &"the"))
                && clause.get(index + 2) == Some(&"meal")))
        .then_some(index)
    })
}

fn is_bare_ingredient(clause: &[&str]) -> bool {
    let mut words = &clause[..whole_meal_qualifier_start(clause).unwrap_or(clause.len())];
    loop {
        if KEYWORD_ALIASES
            .iter()
            .any(|(alias, _)| words.iter().copied().eq(alias.split_whitespace()))
            || FOOD_TAG_VOCABULARY
                .iter()
                .any(|tag| words.iter().copied().eq(tag.split('_')))
        {
            return true;
        }
        if matches!(
            words.first(),
            Some(&"any" | &"added" | &"scrambled" | &"boiled" | &"fried" | &"poached")
        ) {
            words = &words[1..];
        } else {
            return false;
        }
    }
}

struct IngredientClause<'a> {
    words: &'a [&'a str],
    inherited_negative: bool,
    whole_meal: bool,
    explicit_whole_meal: bool,
}

impl IngredientClause<'_> {
    fn exclusion_scope(&self, start: usize, words: usize) -> (bool, bool) {
        let (negative, whole_meal) = exclusion_scope(self.words, start, words);
        (
            negative || self.inherited_negative,
            whole_meal || self.whole_meal,
        )
    }

    fn evidence_text(&self, words: &[&str]) -> String {
        let prefix = if !self.inherited_negative {
            ""
        } else if self.whole_meal {
            "without "
        } else {
            "ingredient without "
        };
        let suffix = if self.explicit_whole_meal && whole_meal_qualifier_start(words).is_none() {
            " anywhere"
        } else {
            ""
        };
        let mut text = String::with_capacity(
            prefix.len()
                + words.iter().map(|word| word.len()).sum::<usize>()
                + words.len().saturating_sub(1)
                + suffix.len(),
        );
        text.push_str(prefix);
        for (index, word) in words.iter().enumerate() {
            if index != 0 {
                text.push(' ');
            }
            text.push_str(word);
        }
        text.push_str(suffix);
        text
    }
}

/// Split components, carrying a negative determiner through bare ingredient
/// lists but not through a new affirmative dish ("and an omelette", "with cream").
fn ingredient_clauses<'a>(tokens: &'a [&'a str]) -> Vec<IngredientClause<'a>> {
    let mut clauses: Vec<IngredientClause<'a>> = Vec::new();
    let mut inherited_scope: Option<(usize, bool, bool)> = None;
    let mut offset = 0;
    for clause in
        tokens.split(|token| matches!(*token, "|" | "with" | "and" | "or" | "but" | "plus"))
    {
        let separator = if offset == 0 { "" } else { tokens[offset - 1] };
        offset += clause.len() + 1;
        if clause.is_empty() {
            if !matches!(separator, "and" | "or") {
                inherited_scope = None;
            }
            continue;
        }
        let carry = inherited_scope
            .filter(|_| matches!(separator, "and" | "or") && is_bare_ingredient(clause));
        let explicit_whole_meal = whole_meal_qualifier_start(clause).is_some();
        let (whole_meal, explicit_whole_meal) = if let Some((start, whole, explicit)) = carry {
            let explicit = explicit || explicit_whole_meal;
            let whole = whole || explicit;
            if explicit {
                for previous in &mut clauses[start..] {
                    previous.whole_meal = true;
                    previous.explicit_whole_meal = true;
                }
            }
            inherited_scope = Some((start, whole, explicit));
            (whole, explicit)
        } else {
            let negative = clause
                .iter()
                .rposition(|word| matches!(*word, "no" | "without" | "not"))
                .filter(|index| is_bare_ingredient(&clause[index + 1..]));
            let whole = explicit_whole_meal
                || negative.is_some_and(|index| index == 0 && clause[index] != "not");
            inherited_scope = negative.map(|_| (clauses.len(), whole, explicit_whole_meal));
            (whole, explicit_whole_meal)
        };
        clauses.push(IngredientClause {
            words: clause,
            inherited_negative: carry.is_some(),
            whole_meal,
            explicit_whole_meal,
        });
    }
    clauses
}

/// Ingredient evidence is clause-local: an egg-free omelette is not egg evidence,
/// but a separate side omelette still is. Only explicit exclusions veto model tags.
fn food_tag_evidence(
    description: &str,
) -> (
    [bool; FOOD_TAG_VOCABULARY.len()],
    [bool; FOOD_TAG_VOCABULARY.len()],
) {
    let normalized = padded_tokens(description);
    let tokens: Vec<_> = normalized.split_whitespace().collect();
    let mut present = [false; FOOD_TAG_VOCABULARY.len()];
    let mut excluded = [false; FOOD_TAG_VOCABULARY.len()];
    let mut whole_meal_excluded = [false; FOOD_TAG_VOCABULARY.len()];
    let mut explicit_whole_meal_excluded = [false; FOOD_TAG_VOCABULARY.len()];
    for parsed in ingredient_clauses(&tokens) {
        let clause = parsed.words;
        let mut clause_present = [false; FOOD_TAG_VOCABULARY.len()];
        let mut clause_excluded = [false; FOOD_TAG_VOCABULARY.len()];
        let explicit_whole_meal = parsed.explicit_whole_meal;
        for &(alias, tag) in KEYWORD_ALIASES {
            let index = FOOD_TAG_VOCABULARY
                .iter()
                .position(|known| *known == tag)
                .unwrap();
            let words = alias.split_whitespace().count();
            for (start, window) in clause.windows(words).enumerate() {
                if !window.iter().copied().eq(alias.split_whitespace()) {
                    continue;
                }
                // Plant milks are not affirmative evidence of dairy.
                if alias == "milk"
                    && start > 0
                    && matches!(
                        clause[start - 1],
                        "coconut" | "soy" | "oat" | "almond" | "rice" | "cashew" | "pea" | "hemp"
                    )
                {
                    continue;
                }
                let (negated, whole_meal) = parsed.exclusion_scope(start, words);
                if negated {
                    clause_excluded[index] = true;
                    // Global facts override guesses; named components stay scoped.
                    whole_meal_excluded[index] |= whole_meal;
                    explicit_whole_meal_excluded[index] |= explicit_whole_meal;
                } else {
                    clause_present[index] = true;
                }
            }
        }
        // Users can exclude a tag category directly, even when its name is
        // not an ingredient alias (e.g. "no dairy" or "no added sugar").
        for (index, tag) in FOOD_TAG_VOCABULARY.iter().enumerate() {
            let words = tag.split('_').count();
            for (start, window) in clause.windows(words).enumerate() {
                if !window.iter().copied().eq(tag.split('_')) {
                    continue;
                }
                let (negated, whole_meal) = parsed.exclusion_scope(start, words);
                if negated {
                    clause_excluded[index] = true;
                    whole_meal_excluded[index] |= whole_meal;
                    explicit_whole_meal_excluded[index] |= explicit_whole_meal;
                } else if clause
                    .iter()
                    .any(|word| matches!(*word, "add" | "added" | "contains"))
                {
                    clause_present[index] = true;
                }
            }
        }
        // Excluding milk from a paneer preparation does not exclude paneer.
        let dairy = FOOD_TAG_VOCABULARY
            .iter()
            .position(|tag| *tag == "dairy")
            .unwrap();
        if clause
            .iter()
            .enumerate()
            .any(|(start, word)| *word == "paneer" && !parsed.exclusion_scope(start, 1).0)
            && !clause
                .windows(2)
                .any(|words| matches!(words[0], "no" | "without" | "not") && words[1] == "dairy")
        {
            clause_excluded[dairy] = false;
        }
        for index in 0..FOOD_TAG_VOCABULARY.len() {
            excluded[index] |= clause_excluded[index];
            present[index] |= clause_present[index] && !clause_excluded[index];
            if clause_present[index] && !clause_excluded[index] {
                whole_meal_excluded[index] = false;
                if clause.iter().any(|word| matches!(*word, "add" | "added")) {
                    explicit_whole_meal_excluded[index] = false;
                }
            }
        }
    }
    for index in 0..FOOD_TAG_VOCABULARY.len() {
        present[index] &= !whole_meal_excluded[index] && !explicit_whole_meal_excluded[index];
    }
    (present, excluded)
}

/// Retain explicit negative correction facts, not historical positive guesses.
/// Latest facts are appended last so ingredient additions/removals supersede
/// earlier exclusions while portion-only changes leave them active.
pub fn reconcile_food_tag_context(
    canonical_description: &str,
    previous_facts: &str,
    latest_facts: &str,
) -> String {
    let normalized = padded_tokens(previous_facts);
    let tokens: Vec<_> = normalized.split_whitespace().collect();
    let mut retained: Vec<(String, usize)> = Vec::new();
    for parsed in ingredient_clauses(&tokens) {
        let clause = parsed.words;
        let text = parsed.evidence_text(clause);
        let (positive, negative) = food_tag_evidence(&text);
        // Historical positives only retire matching exclusions; their ingredient
        // evidence is never copied into the returned context.
        retained.retain(|(fact, index)| {
            if !positive[*index] {
                return true;
            }
            let previous: Vec<_> = fact.split_whitespace().collect();
            let global = (previous.len() == FOOD_TAG_VOCABULARY[*index].split('_').count() + 1
                && previous.first() != Some(&"not"))
                || previous.contains(&"anywhere")
                || previous
                    .windows(3)
                    .any(|words| words == ["in", "this", "meal"] || words == ["in", "the", "meal"]);
            let same_component = previous.iter().any(|word| {
                !matches!(*word, "no" | "not" | "without" | "component" | "ingredient")
                    && !KEYWORD_ALIASES.iter().any(|(alias, _)| {
                        alias
                            .split_whitespace()
                            .any(|alias_word| alias_word == *word)
                    })
                    && clause.contains(word)
            });
            !global && !same_component
        });
        if !negative.iter().any(|excluded| *excluded) {
            continue;
        }
        for (index, excluded) in negative.iter().enumerate() {
            if !excluded {
                continue;
            }
            // Keep the component and this negative phrase, never unrelated
            // historical positives or negative facts that were superseded.
            let mut keep = vec![true; clause.len()];
            let mut placeholders = vec![false; clause.len()];
            for &(alias, tag) in KEYWORD_ALIASES {
                let words = alias.split_whitespace().count();
                for (start, window) in clause.windows(words).enumerate() {
                    if window.iter().copied().eq(alias.split_whitespace())
                        && (tag != FOOD_TAG_VOCABULARY[index]
                            || (!parsed.exclusion_scope(start, words).0
                                && !clause.contains(&"free")))
                    {
                        keep[start..start + words].fill(false);
                        if !parsed.exclusion_scope(start, words).0 {
                            placeholders[start] = true;
                        }
                    }
                }
            }
            for tag in FOOD_TAG_VOCABULARY {
                if *tag == FOOD_TAG_VOCABULARY[index] {
                    continue;
                }
                let words = tag.split('_').count();
                for (start, window) in clause.windows(words).enumerate() {
                    if window.iter().copied().eq(tag.split('_')) {
                        keep[start..start + words].fill(false);
                    }
                }
            }
            let words = clause
                .iter()
                .enumerate()
                .filter_map(|(position, word)| {
                    if keep[position] {
                        Some(*word)
                    } else if placeholders[position] {
                        Some("component")
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            let text = parsed.evidence_text(&words);
            retained.push((text, index));
        }
    }
    let mut context = canonical_description.to_string();
    let (latest_positive, _) = food_tag_evidence(latest_facts);
    for (text, index) in retained {
        if latest_positive[index]
            && (text.contains("anywhere")
                || text.contains("in this meal")
                || text.contains("in the meal"))
        {
            continue;
        }
        context.push('\n');
        context.push_str(&text);
    }
    context.push('\n');
    context.push_str(latest_facts);
    context
}

pub fn keyword_tags_for(description: &str) -> Vec<String> {
    let (present, _) = food_tag_evidence(description);
    FOOD_TAG_VOCABULARY
        .iter()
        .zip(present)
        .filter(|(_, present)| *present)
        .map(|(tag, _)| (*tag).to_string())
        .collect()
}

/// Prefer sanitized LLM tags, enforcing explicit exclusions before either source.
pub fn assign_food_tags(
    llm_tags: impl IntoIterator<Item = impl AsRef<str>>,
    description: &str,
) -> AssignedFoodTags {
    let from_llm = sanitize_food_tags(llm_tags);
    let (present, excluded) = food_tag_evidence(description);
    if !from_llm.is_empty() {
        return AssignedFoodTags {
            tags: from_llm
                .into_iter()
                .filter(|tag| {
                    let index = FOOD_TAG_VOCABULARY
                        .iter()
                        .position(|known| *known == tag)
                        .unwrap();
                    !excluded[index] || present[index]
                })
                .collect(),
            source: "llm",
        };
    }
    AssignedFoodTags {
        tags: FOOD_TAG_VOCABULARY
            .iter()
            .zip(present)
            .filter(|(_, present)| *present)
            .map(|(tag, _)| (*tag).to_string())
            .collect(),
        source: "keyword",
    }
}

fn padded_tokens(s: &str) -> String {
    let mut out = String::from(" ");
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '+' {
            out.push(c.to_ascii_lowercase());
        } else if matches!(c, ',' | ';' | ':' | '.' | '\n' | '\r') {
            out.push_str(" | ");
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
    tx: &mut Transaction<'_, Sqlite>,
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
        .execute(&mut **tx)
        .await
        .with_context(|| format!("insert food_log_tags {food_log_id}/{tag}"))?;
    }
    Ok(())
}

pub async fn delete_food_log_tags(
    tx: &mut Transaction<'_, Sqlite>,
    food_log_id: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM food_log_tags WHERE food_log_id = ?")
        .bind(food_log_id)
        .execute(&mut **tx)
        .await
        .context("delete food_log_tags by id")?;
    Ok(())
}

pub async fn delete_food_log_tags_for_member_day(
    tx: &mut Transaction<'_, Sqlite>,
    member_id: &str,
    date: &str,
    timezone: chrono_tz::Tz,
) -> Result<()> {
    let (start, end) = crate::civil_day_bounds_utc(date, timezone)?;
    sqlx::query(
        "DELETE FROM food_log_tags WHERE food_log_id IN (\
            SELECT id FROM food_log \
            WHERE family_member_id = ? AND julianday(timestamp) >= julianday(?) AND julianday(timestamp) < julianday(?)\
         )",
    )
    .bind(member_id)
    .bind(start)
    .bind(end)
    .execute(&mut **tx)
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

    if rows.is_empty() {
        return Ok(0);
    }

    let mut tx = pool.begin().await.context("begin food_log_tags backfill")?;
    let mut tagged = 0u64;
    for (id, description) in rows {
        let assigned = AssignedFoodTags {
            tags: keyword_tags_for(&description),
            source: "keyword",
        };
        if assigned.tags.is_empty() {
            continue;
        }
        insert_food_log_tags(&mut tx, &id, &assigned).await?;
        tagged += 1;
    }
    tx.commit().await.context("commit food_log_tags backfill")?;
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
        assert_eq!(tags, vec!["dairy".to_string(), "caffeine".to_string()]);
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

    #[test]
    fn explicit_egg_exclusions_override_keyword_and_model_tags() {
        for description in [
            "paneer with cream, no eggs",
            "paneer without eggs",
            "egg-free paneer",
            "bhurji is paneer not eggs",
            "egg-free omelette",
            "omelette without eggs",
            "paneer bhurji, not scrambled eggs",
            "scrambled eggs with cream. Correction: there are no eggs in this meal",
            "scrambled eggs. Correction: no eggs anywhere in this meal",
        ] {
            assert!(
                !keyword_tags_for(description)
                    .iter()
                    .any(|tag| tag == "eggs"),
                "{description}"
            );
            let assigned = assign_food_tags(["eggs", "dairy"], description);
            assert_eq!(assigned.tags, vec!["dairy"], "{description}");
            assert_eq!(assigned.source, "llm");
        }
    }

    #[test]
    fn scoped_egg_exclusion_preserves_a_separate_omelette() {
        for description in [
            "bhurji is paneer not eggs, with an omelette on the side",
            "egg-free bhurji and an omelette",
            "paneer without eggs; two eggs on the side",
            "egg-free omelette plus a frittata",
            "bhurji is paneer not scrambled eggs, with an omelette on the side",
        ] {
            assert!(
                keyword_tags_for(description)
                    .iter()
                    .any(|tag| tag == "eggs"),
                "{description}"
            );
            assert_eq!(
                assign_food_tags(["eggs"], description).tags,
                vec!["eggs"],
                "{description}"
            );
        }
    }

    #[test]
    fn exclusions_do_not_turn_substrings_or_unrelated_negation_into_evidence() {
        assert_eq!(keyword_tags_for("eggplant, no eggs"), vec!["nightshades"]);
        assert_eq!(keyword_tags_for("eggs without cream"), vec!["eggs"]);
        assert_eq!(keyword_tags_for("not only eggs"), vec!["eggs"]);
        assert_eq!(
            assign_food_tags(["eggs"], "no eggs").tags,
            Vec::<String>::new()
        );
        assert_eq!(
            assign_food_tags(["unknown"], "cream, no eggs").tags,
            vec!["dairy"]
        );
        assert_eq!(
            assign_food_tags(["eggs"], "egg bhurji\nno eggs").tags,
            Vec::<String>::new()
        );
        assert_eq!(
            keyword_tags_for("omelette\nwithout eggs"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn explicit_category_exclusions_survive_plant_milk_and_model_guesses() {
        let description = "creamy curry; no dairy; lentils and coconut milk";
        assert!(assign_food_tags(["dairy"], description).tags.is_empty());
        assert!(!keyword_tags_for(description)
            .iter()
            .any(|tag| tag == "dairy"));
        assert_eq!(keyword_tags_for("soy milk"), vec!["soy"]);
        assert!(keyword_tags_for("oat milk").is_empty());
        assert!(assign_food_tags(["gluten"], "bread; no gluten")
            .tags
            .is_empty());
        assert!(assign_food_tags(["added_sugar"], "cake; no added sugar")
            .tags
            .is_empty());
        assert_eq!(
            assign_food_tags(["dairy"], "curry without dairy; a side of cream").tags,
            vec!["dairy"]
        );
    }

    #[test]
    fn newer_facts_supersede_earlier_unqualified_exclusions() {
        assert_eq!(
            assign_food_tags(["eggs"], "paneer\nno eggs\nI added two eggs").tags,
            vec!["eggs"]
        );
        assert_eq!(
            assign_food_tags(["eggs"], "egg bhurji\nI added two eggs\nno eggs").tags,
            Vec::<String>::new()
        );
    }

    #[test]
    fn trailing_not_fragment_stays_with_its_component() {
        for description in [
            "omelette on the side and bhurji is paneer, not eggs",
            "bhurji is paneer, not eggs; an omelette on the side",
        ] {
            assert!(keyword_tags_for(description)
                .iter()
                .any(|tag| tag == "eggs"));
            assert_eq!(assign_food_tags(["eggs"], description).tags, vec!["eggs"]);
        }
        assert!(assign_food_tags(
            ["eggs"],
            "omelette on the side; no eggs anywhere in this meal"
        )
        .tags
        .is_empty());
        assert!(assign_food_tags(
            ["eggs"],
            "no eggs anywhere in this meal; an omelette on the side"
        )
        .tags
        .is_empty());
        assert!(
            keyword_tags_for("no eggs anywhere in this meal; an omelette on the side").is_empty()
        );
    }

    #[test]
    fn coordinated_exclusions_stop_at_affirmative_components() {
        for conjunction in ["and", "or"] {
            let description = format!("lentils without eggs {conjunction} milk");
            assert!(assign_food_tags(["eggs", "dairy"], &description)
                .tags
                .is_empty());
            assert!(keyword_tags_for(&description).is_empty());
            let description = format!("paneer without eggs {conjunction} milk");
            assert_eq!(
                assign_food_tags(["eggs", "dairy"], &description).tags,
                vec!["dairy"]
            );
            assert_eq!(keyword_tags_for(&description), vec!["dairy"]);
        }
        for description in [
            "lentils without eggs and with cream on the side",
            "cream on the side and lentils without eggs",
        ] {
            assert_eq!(
                assign_food_tags(["eggs", "dairy"], description).tags,
                vec!["dairy"]
            );
            assert_eq!(keyword_tags_for(description), vec!["dairy"]);
        }
        for description in [
            "lentils without eggs and an omelette on the side",
            "an omelette on the side and lentils without eggs",
        ] {
            assert_eq!(assign_food_tags(["eggs"], description).tags, vec!["eggs"]);
            assert_eq!(keyword_tags_for(description), vec!["eggs"]);
        }
        for description in [
            "omelette on the side and paneer without milk or eggs",
            "paneer without milk or eggs and an omelette on the side",
        ] {
            assert_eq!(
                assign_food_tags(["dairy", "eggs"], description).tags,
                ["dairy", "eggs"]
            );
            assert_eq!(keyword_tags_for(description), ["dairy", "eggs"]);
        }
    }

    #[test]
    fn coordinated_whole_meal_exclusions_veto_earlier_guesses_and_survive_portion_changes() {
        for conjunction in ["and", "or"] {
            for ingredients in [["dairy", "eggs"], ["eggs", "dairy"]] {
                for qualifier in ["", " anywhere in this meal", " in the meal"] {
                    let facts = format!(
                        "no {} {conjunction} {}{qualifier}",
                        ingredients[0], ingredients[1]
                    );
                    let description = format!("scrambled eggs with milk\n{facts}");
                    assert!(
                        assign_food_tags(["dairy", "eggs"], &description)
                            .tags
                            .is_empty(),
                        "{description}"
                    );
                    assert!(keyword_tags_for(&description).is_empty(), "{description}");
                    let context =
                        reconcile_food_tag_context("scrambled eggs with milk", &facts, "ate half");
                    assert!(
                        assign_food_tags(["dairy", "eggs"], &context)
                            .tags
                            .is_empty(),
                        "{context}"
                    );
                    assert!(keyword_tags_for(&context).is_empty(), "{context}");
                }
            }
        }
        for (description, tags) in [
            ("scrambled eggs with added sugar\nno any eggs or added sugar anywhere in this meal", ["eggs", "added_sugar"]),
            ("bhurji without eggs or dairy anywhere in this meal; an omelette and cream on the side", ["dairy", "eggs"]),
        ] {
            assert!(assign_food_tags(tags, description).tags.is_empty(), "{description}");
            assert!(keyword_tags_for(description).is_empty(), "{description}");
        }
    }

    #[test]
    fn retained_exclusions_survive_portion_changes_and_drop_old_positives() {
        let context =
            reconcile_food_tag_context("cream curry", "no dairy", "make that half a bowl");
        assert!(assign_food_tags(["dairy"], &context).tags.is_empty());
        assert!(keyword_tags_for(&context).is_empty());
        let context = reconcile_food_tag_context(
            "lentil curry",
            "I had eggs and cream",
            "no eggs and no dairy",
        );
        assert!(assign_food_tags(["eggs", "dairy"], &context)
            .tags
            .is_empty());
        assert!(keyword_tags_for(&context).is_empty());
        let context = reconcile_food_tag_context(
            "lentil curry",
            "I had eggs and cream; no eggs; no dairy",
            "half a bowl",
        );
        assert!(assign_food_tags(["eggs", "dairy"], &context)
            .tags
            .is_empty());
        assert!(keyword_tags_for(&context).is_empty());
    }

    #[test]
    fn latest_additions_override_only_their_category() {
        let context =
            reconcile_food_tag_context("lentil curry", "no eggs; no dairy", "I added two eggs");
        assert_eq!(
            assign_food_tags(["eggs", "dairy"], &context).tags,
            vec!["eggs"]
        );
        assert_eq!(keyword_tags_for(&context), vec!["eggs"]);
        let context = reconcile_food_tag_context(
            "lentil curry",
            "no eggs; no dairy",
            "with cream on the side",
        );
        assert_eq!(
            assign_food_tags(["eggs", "dairy"], &context).tags,
            vec!["dairy"]
        );
        assert_eq!(keyword_tags_for(&context), vec!["dairy"]);
    }

    #[test]
    fn explicit_addition_supersedes_historical_whole_meal_exclusion() {
        let context = reconcile_food_tag_context(
            "lentil curry",
            "no dairy anywhere in this meal; no eggs",
            "with cream on the side",
        );
        assert_eq!(
            assign_food_tags(["eggs", "dairy"], &context).tags,
            vec!["dairy"]
        );
        assert_eq!(keyword_tags_for(&context), vec!["dairy"]);
        let context = reconcile_food_tag_context(
            "lentil curry with cream",
            "no dairy; I added cream",
            "half a bowl",
        );
        assert_eq!(assign_food_tags(["dairy"], &context).tags, vec!["dairy"]);
        assert_eq!(keyword_tags_for(&context), vec!["dairy"]);
    }

    #[test]
    fn retained_scoped_exclusions_preserve_unaffected_side_dishes() {
        for canonical in [
            "bhurji and an omelette on the side",
            "an omelette on the side and bhurji",
        ] {
            let context = reconcile_food_tag_context(
                canonical,
                "bhurji is paneer, not eggs",
                "half as much bhurji",
            );
            assert_eq!(assign_food_tags(["eggs"], &context).tags, vec!["eggs"]);
            assert!(keyword_tags_for(&context).iter().any(|tag| tag == "eggs"));
        }
    }
}
