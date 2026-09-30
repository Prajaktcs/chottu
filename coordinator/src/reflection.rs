use anyhow::{Context, Result};
use chotu_common::{AppConfig, ChotuLlm, HealthCondition, HealthFamilySummary};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow)]
pub struct SimpleTx {
    pub merchant: String,
    pub amount: f64,
    pub category: String,
    pub currency: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionCheckin {
    pub condition_id: String,
    pub label: String,
    pub score: u8,
    pub note: Option<String>,
}

/// Household reflections must never carry a member's condition definitions.
pub fn checkin_conditions(config: &AppConfig, member_id: Option<&str>) -> Vec<HealthCondition> {
    member_id
        .and_then(|id| {
            config
                .family
                .members
                .iter()
                .find(|m| m.id.eq_ignore_ascii_case(id))
        })
        .map(|member| {
            member
                .health_conditions
                .iter()
                .filter(|c| c.check_in && !c.id.trim().is_empty() && !c.label.trim().is_empty())
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

pub fn append_checkin_questions(prompt: &mut String, conditions: &[HealthCondition]) {
    if conditions.is_empty() {
        return;
    }
    prompt.push_str("\n\nOptional symptom check-in (0 = calm, 5 = worst flare). Reply on a separate line for each condition, with an optional note; skipping is fine:\n");
    for condition in conditions {
        prompt.push_str(&format!(
            "{} today, 0–5? Reply: {}: <0–5> [note]\n",
            condition.label, condition.id
        ));
    }
}

/// Explicit condition lines avoid mistaking ordinary journal numbers for scores.
pub fn parse_condition_checkins(
    response: &str,
    conditions: &[HealthCondition],
) -> Vec<ConditionCheckin> {
    let mut checkins = Vec::new();
    for condition in conditions {
        let mut answer = None;
        for line in response.lines() {
            let line = line.trim().trim_start_matches("- ").trim();
            let payload = line
                .split_once(':')
                .and_then(|(name, rest)| {
                    (name.trim().eq_ignore_ascii_case(&condition.id)
                        || name.trim().eq_ignore_ascii_case(&condition.label))
                    .then_some(rest.trim())
                })
                .or_else(|| {
                    // A bare number is allowed only as the entire single-condition reply.
                    // Notes use the explicit condition prefix to avoid journal prose like "3 meetings".
                    (conditions.len() == 1 && response.trim() == line && line.len() == 1)
                        .then_some(line)
                });
            let Some(payload) = payload else { continue };
            let (score, note) = payload
                .split_once(char::is_whitespace)
                .unwrap_or((payload, ""));
            if score.len() != 1 || !matches!(score.as_bytes()[0], b'0'..=b'5') {
                continue;
            }
            answer = Some(ConditionCheckin {
                condition_id: condition.id.clone(),
                label: condition.label.clone(),
                score: score.as_bytes()[0] - b'0',
                note: (!note.trim().is_empty()).then(|| note.trim().to_string()),
            });
        }
        if let Some(answer) = answer {
            checkins.push(answer);
        }
    }
    checkins
}

pub async fn save_condition_checkins(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
    checkins: &[ConditionCheckin],
    conditions: &[HealthCondition],
) -> Result<Vec<ConditionCheckin>> {
    let mut tx = pool.begin().await?;
    for checkin in checkins {
        sqlx::query("INSERT INTO condition_checkin (family_member_id, date, condition_id, score, note) VALUES (?, ?, ?, ?, ?) ON CONFLICT(family_member_id, date, condition_id) DO UPDATE SET score = excluded.score, note = excluded.note")
            .bind(member_id).bind(date).bind(&checkin.condition_id)
            .bind(checkin.score as i32).bind(&checkin.note)
            .execute(&mut *tx).await.context("Failed to save symptom check-in")?;
    }
    // Rehydrate today's scores so a later reflection that skips a question
    // does not erase its previously recorded score from the journal / RAG.
    let rows: Vec<(String, i32, Option<String>)> = sqlx::query_as(
        "SELECT condition_id, score, note FROM condition_checkin WHERE family_member_id = ? AND date = ?",
    ).bind(member_id).bind(date).fetch_all(&mut *tx).await?;
    let saved = conditions
        .iter()
        .filter_map(|condition| {
            let (_, score, note) = rows.iter().find(|(id, _, _)| id == &condition.id)?;
            let score = u8::try_from(*score).ok().filter(|score| *score <= 5)?;
            Some(ConditionCheckin {
                condition_id: condition.id.clone(),
                label: condition.label.clone(),
                score,
                note: note.clone(),
            })
        })
        .collect();
    tx.commit().await?;
    Ok(saved)
}

fn format_checkin_journal(checkins: &[ConditionCheckin]) -> String {
    let mut section = String::new();
    if !checkins.is_empty() {
        // Memory indexes the ## Response section and stops at the next ##.
        section.push_str("### Condition Check-ins\n");
        for checkin in checkins {
            section.push_str(&format!(
                "- {} ({}): {}/5",
                checkin.label, checkin.condition_id, checkin.score
            ));
            if let Some(note) = &checkin.note {
                section.push_str(&format!(" — {note}"));
            }
            section.push('\n');
        }
        section.push_str("\n### Journal\n");
    }
    section
}

pub async fn get_daily_data(
    pool: &SqlitePool,
    date: &str,
    config: &chotu_common::AppConfig,
) -> Result<(Vec<SimpleTx>, Vec<HealthFamilySummary>)> {
    let (start, end) = chotu_common::civil_day_bounds_utc(date, config.resolved_tz())?;
    // Query financials
    let txs = sqlx::query_as::<_, SimpleTx>(
        r#"
        SELECT merchant, amount, category, currency
        FROM financial_ledger
        WHERE julianday(timestamp) >= julianday(?) AND julianday(timestamp) < julianday(?)
        "#,
    )
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
    .context("Failed to query daily financials")?;

    // Query health summaries
    let db_healths = sqlx::query_as::<_, HealthFamilySummary>(
        r#"
        SELECT *
        FROM health_family_summary
        WHERE date = ?
        "#,
    )
    .bind(date)
    .fetch_all(pool)
    .await
    .context("Failed to query daily health summaries")?;

    // Map DB healths to a HashMap
    let mut health_map: std::collections::HashMap<String, HealthFamilySummary> = db_healths
        .into_iter()
        .map(|h| (h.family_member_id.clone(), h))
        .collect();

    // Ensure all configured family members are represented (fill with defaults if missing)
    let mut healths = Vec::new();
    for member in &config.family.members {
        if let Some(h) = health_map.remove(&member.id) {
            healths.push(h);
        } else {
            healths.push(HealthFamilySummary {
                date: date.to_string(),
                family_member_id: member.id.clone(),
                total_calories_ingested: 0,
                protein_grams: 0.0,
                carbs_grams: 0.0,
                fats_grams: 0.0,
                step_count: 0,
                active_calories_burned: 0,
                sleep_hours: None,
                perceived_energy: None,
                omega_3_dha_mg: 0.0,
                cholesterol_mg: 0.0,
                saturated_fat_g: 0.0,
                unsaturated_fat_g: 0.0,
                triglycerides_mg: 0.0,
                iron_mg: 0.0,
                vitamin_b_mg: 0.0,
                vitamin_c_mg: 0.0,
                sugar_g: 0.0,
                fiber_g: 0.0,
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
            });
        }
    }

    // Also include any extra records in DB that are not in config.yaml
    for (_, h) in health_map {
        healths.push(h);
    }

    Ok((txs, healths))
}

pub async fn generate_reflection_prompt(
    llm: &ChotuLlm,
    txs: &[SimpleTx],
    healths: &[HealthFamilySummary],
    date: &str,
    core_values: Option<&chotu_common::CoreValues>,
) -> Result<String> {
    // Format the logs to feed to LLM
    let mut logs_summary = String::new();
    logs_summary.push_str("=== Financial Transactions ===\n");
    if txs.is_empty() {
        logs_summary.push_str("No transactions logged today.\n");
    } else {
        for tx in txs {
            logs_summary.push_str(&format!(
                "- spent {:.2} {} at {} (Category: {})\n",
                tx.amount, tx.currency, tx.merchant, tx.category
            ));
        }
    }

    logs_summary.push_str("\n=== Family Health & Nutrition Metrics ===\n");
    if healths.is_empty() {
        logs_summary.push_str("No health telemetry logs for any family member today.\n");
    } else {
        for h in healths {
            let sleep = h
                .sleep_hours
                .map(|s| format!("{s}"))
                .unwrap_or_else(|| "N/A".to_string());
            let energy = h
                .perceived_energy
                .map(|e| e.to_string())
                .unwrap_or_else(|| "N/A".to_string());
            logs_summary.push_str(&format!(
                "- Member: {}\n  * Nutrition: {} kcal ingested (Protein: {}g, Carbs: {}g, Fats: {}g)\n  * Activity: {} steps, {} active calories burned\n  * Sleep: {} hrs\n  * Energy Level: {}\n",
                h.family_member_id,
                h.total_calories_ingested,
                h.protein_grams,
                h.carbs_grams,
                h.fats_grams,
                h.step_count,
                h.active_calories_burned,
                sleep,
                energy
            ));
        }
    }

    let values = core_values
        .cloned()
        .unwrap_or_else(chotu_common::CoreValues::default);
    let values_block = format_core_values_for_prompt(&values);

    let system_prompt = "You are Chotu's Evening Reflection Engine. Each night you write a short \
journal prompt that does TWO jobs equally — do not drop either:\n\
1) HEALTH: Ground in today's nutrition, steps/activity, sleep, and energy when those logs exist. \
Cite specific numbers or trends (high/low protein, steps, sleep hours, late eating, perceived energy). \
If health logs are empty, say so briefly and skip invented metrics.\n\
2) VALUES: Train the user to solidify and live their two core values (Growth + Contribution by default). \
Integrity is the alignment sensor; courage fuels Growth; humility guards Contribution — do not list those as separate core values. \
Prefer one values lens per night from the practice list (unspoken/heavy-body, ego autopsy, 'I don't know', bring-a-brick, silence-as-withholding).\n\n\
Spend/financial logs are optional supporting color when clearly relevant.\n\n\
Write 2–4 sentences, then 1–2 sharp questions that cover both health AND values (one question can combine them). \
Be concrete — no pep talk, therapy clichés, or inventing events. \
Do not include reasoning, frontmatter, or commentary. Only return the final reflection prompt.";

    let user_prompt = format!(
        "Date: {}\n\n=== Core Values (operating system) ===\n{}\n\n=== Today's logs ===\n{}",
        date, values_block, logs_summary
    );

    let prompt = llm
        .generate_prompt(system_prompt, &user_prompt)
        .await
        .map_err(|e| anyhow::anyhow!("LLM error: {:?}", e))?;

    // If DeepSeek-R1 returned thought blocks, strip them out (anything between <think> and </think>)
    let cleaned_prompt = strip_think_blocks(&prompt);

    Ok(cleaned_prompt)
}

fn format_core_values_for_prompt(values: &chotu_common::CoreValues) -> String {
    let mut out = String::new();
    out.push_str("Anchors:\n");
    for a in &values.anchors {
        out.push_str(&format!("- {}: {}\n", a.name, a.definition));
    }
    if let Some(note) = &values.integrity_note {
        out.push_str(&format!("\nIntegrity / tools:\n{}\n", note));
    }
    if !values.practices.is_empty() {
        out.push_str("\nPractice lenses (pick what fits tonight):\n");
        for p in &values.practices {
            out.push_str(&format!("- {}\n", p));
        }
    }
    out
}

fn strip_think_blocks(text: &str) -> String {
    let mut output = String::new();
    let mut remaining = text;
    while let Some(start_idx) = remaining.find("<think>") {
        output.push_str(&remaining[..start_idx]);
        if let Some(end_idx) = remaining.find("</think>") {
            remaining = &remaining[end_idx + 8..];
        } else {
            // Unclosed think tag, skip the rest
            remaining = "";
            break;
        }
    }
    output.push_str(remaining);
    output.trim().to_string()
}

/// Keep only the linked member's health records for a private reflection.
/// `None` is the configured household group and retains household-wide data.
pub fn filter_health_for_member(healths: &mut Vec<HealthFamilySummary>, member_id: Option<&str>) {
    if let Some(member_id) = member_id {
        healths.retain(|health| health.family_member_id.eq_ignore_ascii_case(member_id));
    }
}

fn encoded_member_component(member_id: &str) -> String {
    let mut encoded = String::with_capacity(member_id.len());
    for byte in member_id.trim().bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

fn reflection_file_path(
    brain_path: &PathBuf,
    date: &str,
    member_id: Option<&str>,
) -> Result<PathBuf> {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(anyhow::anyhow!(
            "Invalid date format for reflection save: {}",
            date
        ));
    }
    let file_name = match member_id.filter(|member| !member.trim().is_empty()) {
        Some(member) => format!("{}--{}.md", date, encoded_member_component(member)),
        None => format!("{}.md", date),
    };
    Ok(brain_path
        .join("Journal")
        .join(parts[0])
        .join(parts[1])
        .join(file_name))
}

pub async fn save_reflection(
    date: &str,
    prompt: &str,
    response: &str,
    txs: &[SimpleTx],
    healths: &[HealthFamilySummary],
    member_id: Option<&str>,
    checkins: &[ConditionCheckin],
) -> Result<PathBuf> {
    // Retrieve journal directory from env or default to ~/chotu_brain
    let brain_dir_str =
        std::env::var("CHOTU_BRAIN_DIR").unwrap_or_else(|_| "~/chotu_brain".to_string());

    // Resolve home directory
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/user".to_string());
    let brain_path = PathBuf::from(brain_dir_str.replace("~", &home));

    // Direct-message journals include the member id so same-day entries cannot collide.
    let file_path = reflection_file_path(&brain_path, date, member_id)?;
    let target_dir = file_path
        .parent()
        .expect("reflection file path always has a parent");
    tokio::fs::create_dir_all(target_dir)
        .await
        .context("Failed to create target reflection journal directory")?;

    // Format the YAML frontmatter
    let mut content = String::new();
    content.push_str("---\n");
    content.push_str(&format!("date: {}\n", date));
    if let Some(mid) = member_id.filter(|s| !s.trim().is_empty()) {
        content.push_str(&format!(
            "member: \"{}\"\n",
            escape_yaml_double_quoted(mid.trim())
        ));
    }

    // Escape prompt text for YAML double quotes
    let escaped_prompt = escape_yaml_double_quoted(&prompt.replace('\n', " "));
    content.push_str(&format!("prompt: \"{}\"\n", escaped_prompt));

    content.push_str("financials:\n");
    let total_spent: f64 = txs.iter().map(|tx| tx.amount).sum();
    content.push_str(&format!("  total_spent: {:.2}\n", total_spent));
    content.push_str("  transactions:\n");
    for tx in txs {
        content.push_str(&format!(
            "    - merchant: \"{}\"\n",
            escape_yaml_double_quoted(&tx.merchant)
        ));
        content.push_str(&format!("      amount: {:.2}\n", tx.amount));
        content.push_str(&format!(
            "      category: \"{}\"\n",
            escape_yaml_double_quoted(&tx.category)
        ));
        content.push_str(&format!(
            "      currency: \"{}\"\n",
            escape_yaml_double_quoted(&tx.currency)
        ));
    }

    content.push_str("health:\n");
    for h in healths {
        content.push_str(&format!(
            "  \"{}\":\n",
            escape_yaml_double_quoted(&h.family_member_id)
        ));
        content.push_str(&format!(
            "    calories_ingested: {}\n",
            h.total_calories_ingested
        ));
        content.push_str(&format!("    protein_grams: {}\n", h.protein_grams));
        content.push_str(&format!("    carbs_grams: {}\n", h.carbs_grams));
        content.push_str(&format!("    fats_grams: {}\n", h.fats_grams));
        content.push_str(&format!("    steps: {}\n", h.step_count));
        content.push_str(&format!(
            "    active_calories_burned: {}\n",
            h.active_calories_burned
        ));
        match h.sleep_hours {
            Some(s) => content.push_str(&format!("    sleep: {}\n", s)),
            None => content.push_str("    sleep: null\n"),
        }
        match h.perceived_energy {
            Some(e) => content.push_str(&format!("    perceived_energy: {}\n", e)),
            None => content.push_str("    perceived_energy: null\n"),
        }
    }
    content.push_str("---\n\n");

    content.push_str("# Evening Reflection\n\n");
    content.push_str("## Prompt\n");
    content.push_str(prompt);
    content.push_str("\n\n");
    content.push_str("## Response\n");
    content.push_str(&format_checkin_journal(checkins));
    content.push_str(response);
    content.push('\n');

    tokio::fs::write(&file_path, content)
        .await
        .with_context(|| format!("Failed to write daily reflection file to {:?}", file_path))?;

    Ok(file_path)
}

fn escape_yaml_double_quoted(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn daily_financials_use_configured_civil_day_instead_of_utc_date() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let mut config = AppConfig::default();
        config.timezone = Some("America/Toronto".into());
        for (id, timestamp) in [
            ("before", "2026-09-29T03:59:59Z"),
            ("first", "2026-09-29 04:00:00+00:00"),
            ("evening", "2026-09-30T02:00:00Z"),
            ("after", "2026-09-30T00:00:00-04:00"),
        ] {
            sqlx::query("INSERT INTO financial_ledger (id, timestamp, amount, currency, institution, merchant, category, source_type) VALUES (?, ?, 10, 'CAD', 'bank', ?, 'food', 'EMAIL_STREAM')")
                .bind(id).bind(timestamp).bind(id).execute(&pool).await.unwrap();
        }
        let (txs, _) = get_daily_data(&pool, "2026-09-29", &config).await.unwrap();
        let mut merchants: Vec<_> = txs.into_iter().map(|t| t.merchant).collect();
        merchants.sort();
        assert_eq!(merchants, vec!["evening", "first"]);
    }

    fn condition(id: &str, label: &str) -> HealthCondition {
        HealthCondition {
            id: id.into(),
            label: label.into(),
            check_in: true,
            lag_window: [1, 3],
            notes: None,
        }
    }

    #[test]
    fn checkin_prompts_are_private_and_opt_in() {
        let mut config = AppConfig::default();
        let active = condition("skin", "Skin symptoms");
        let mut disabled = condition("sleep", "Sleep symptoms");
        disabled.check_in = false;
        config.family.members[0].health_conditions = vec![active.clone(), disabled];
        let mut other = config.family.members[0].clone();
        other.id = "jordan".into();
        other.health_conditions = vec![condition("other", "Private other condition")];
        config.family.members.push(other);

        assert!(checkin_conditions(&config, None).is_empty());
        assert!(checkin_conditions(&config, Some("unknown")).is_empty());
        let selected = checkin_conditions(&config, Some("alex"));
        assert_eq!(selected, vec![active]);
        let mut prompt = "Original journal prompt".to_string();
        append_checkin_questions(&mut prompt, &selected);
        assert!(prompt.starts_with("Original journal prompt"));
        assert!(prompt.contains("skin: <0–5> [note]"));
        assert!(!prompt.contains("Sleep symptoms"));
        assert!(!prompt.contains("Private other condition"));
        let mut household = "Household prompt".to_string();
        append_checkin_questions(&mut household, &checkin_conditions(&config, None));
        assert_eq!(household, "Household prompt");
    }

    #[test]
    fn checkins_parse_named_conditions_and_preserve_notes() {
        let conditions = vec![
            condition("skin", "Skin symptoms"),
            condition("joint", "Joint pain"),
        ];
        let response = "I slept 5 hours and walked 3 miles.\n- SKIN: 0\nJoint pain: 5 itch and stress\nunknown: 2\nskin: 2 better tonight";
        let checkins = parse_condition_checkins(response, &conditions);
        assert_eq!(checkins.len(), 2);
        assert_eq!(checkins[0].score, 2);
        assert_eq!(checkins[0].note.as_deref(), Some("better tonight"));
        assert_eq!(checkins[1].score, 5);
        assert_eq!(checkins[1].note.as_deref(), Some("itch and stress"));
        let journal = format_checkin_journal(&checkins);
        assert!(journal.contains("Skin symptoms (skin): 2/5 — better tonight"));
        assert!(journal.contains("Joint pain (joint): 5/5 — itch and stress"));
    }

    #[test]
    fn checkins_skip_missing_invalid_and_ambiguous_scores() {
        let one = vec![condition("skin", "Skin symptoms")];
        for reply in [
            "skin: -1",
            "skin: 6",
            "skin: 10",
            "skin: 3.5",
            "skin: 2/5",
            "skin: skip",
            "I slept 5 hours",
            "3 meetings today",
            "unknown: 3",
            "Journal entry\n3",
        ] {
            assert!(parse_condition_checkins(reply, &one).is_empty(), "{reply}");
        }
        assert_eq!(parse_condition_checkins("0", &one)[0].score, 0);
        assert_eq!(parse_condition_checkins("5", &one)[0].score, 5);
        let mut multiple = one;
        multiple.push(condition("joint", "Joint pain"));
        assert!(parse_condition_checkins("3", &multiple).is_empty());
        assert!(parse_condition_checkins("skin: 3", &[]).is_empty());
        assert!(format_checkin_journal(&[]).is_empty());
    }

    #[tokio::test]
    async fn checkins_upsert_without_crossing_member_or_date_boundaries() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::raw_sql(include_str!(
            "../../chotu-common/migrations/20260824000003_condition_tracking.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let conditions = vec![condition("skin", "Skin symptoms")];
        let first = parse_condition_checkins("skin: 3 itch", &conditions);
        save_condition_checkins(&pool, "alex", "2026-09-29", &first, &conditions)
            .await
            .unwrap();
        save_condition_checkins(&pool, "jordan", "2026-09-29", &first, &conditions)
            .await
            .unwrap();
        save_condition_checkins(&pool, "alex", "2026-09-28", &first, &conditions)
            .await
            .unwrap();
        let revised = parse_condition_checkins("skin: 1", &conditions);
        save_condition_checkins(&pool, "alex", "2026-09-29", &revised, &conditions)
            .await
            .unwrap();
        let skipped = save_condition_checkins(&pool, "alex", "2026-09-29", &[], &conditions)
            .await
            .unwrap();
        assert_eq!(skipped, revised);
        assert!(format_checkin_journal(&skipped).contains("Skin symptoms (skin): 1/5"));
        let rows: Vec<(String, String, i32, Option<String>)> = sqlx::query_as("SELECT family_member_id, date, score, note FROM condition_checkin ORDER BY family_member_id, date")
            .fetch_all(&pool).await.unwrap();
        assert_eq!(
            rows,
            vec![
                ("alex".into(), "2026-09-28".into(), 3, Some("itch".into())),
                ("alex".into(), "2026-09-29".into(), 1, None),
                ("jordan".into(), "2026-09-29".into(), 3, Some("itch".into())),
            ]
        );
    }

    #[tokio::test]
    async fn checkin_batch_failure_keeps_previous_scores_for_retry() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::raw_sql(include_str!(
            "../../chotu-common/migrations/20260824000003_condition_tracking.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let conditions = vec![
            condition("skin", "Skin symptoms"),
            condition("joint", "Joint pain"),
        ];
        let first = parse_condition_checkins("skin: 2", &conditions);
        save_condition_checkins(&pool, "alex", "2026-09-29", &first, &conditions)
            .await
            .unwrap();
        sqlx::raw_sql("CREATE TRIGGER reject_joint BEFORE INSERT ON condition_checkin WHEN NEW.condition_id = 'joint' BEGIN SELECT RAISE(ABORT, 'simulated storage failure'); END;")
            .execute(&pool).await.unwrap();
        let revised = parse_condition_checkins("skin: 4\njoint: 3", &conditions);
        assert!(
            save_condition_checkins(&pool, "alex", "2026-09-29", &revised, &conditions)
                .await
                .is_err()
        );
        let saved = save_condition_checkins(&pool, "alex", "2026-09-29", &[], &conditions)
            .await
            .unwrap();
        assert_eq!(saved, first);
        sqlx::raw_sql("DROP TRIGGER reject_joint")
            .execute(&pool)
            .await
            .unwrap();
        let retried = save_condition_checkins(&pool, "alex", "2026-09-29", &revised, &conditions)
            .await
            .unwrap();
        assert_eq!(retried, revised);
    }

    #[test]
    fn test_strip_think_blocks() {
        let input = "<think>some internal thought process</think>Actual prompt here";
        assert_eq!(strip_think_blocks(input), "Actual prompt here");

        let input_multi =
            "<think>\nthought line 1\nthought line 2\n</think>\n  Actual prompt here\n";
        assert_eq!(strip_think_blocks(input_multi), "Actual prompt here");

        let input_no_think = "Hello world";
        assert_eq!(strip_think_blocks(input_no_think), "Hello world");
    }

    #[test]
    fn format_core_values_includes_anchors_and_practices() {
        let values = chotu_common::CoreValues::default();
        let formatted = format_core_values_for_prompt(&values);
        assert!(formatted.contains("Growth"));
        assert!(formatted.contains("Contribution"));
        assert!(formatted.contains("Practice lenses"));
        assert!(formatted.contains("Ego autopsy") || formatted.contains("ego"));
    }

    #[test]
    fn yaml_double_quote_escapes_member_id_metacharacters() {
        let escaped = escape_yaml_double_quoted("alex: #1\\home\"");
        assert_eq!(escaped, "alex: #1\\\\home\\\"");
    }

    fn health_summary(member_id: &str) -> HealthFamilySummary {
        HealthFamilySummary {
            date: "2026-09-07".to_string(),
            family_member_id: member_id.to_string(),
            total_calories_ingested: 0,
            protein_grams: 0.0,
            carbs_grams: 0.0,
            fats_grams: 0.0,
            step_count: 0,
            active_calories_burned: 0,
            sleep_hours: None,
            perceived_energy: None,
            omega_3_dha_mg: 0.0,
            cholesterol_mg: 0.0,
            saturated_fat_g: 0.0,
            unsaturated_fat_g: 0.0,
            triglycerides_mg: 0.0,
            iron_mg: 0.0,
            vitamin_b_mg: 0.0,
            vitamin_c_mg: 0.0,
            sugar_g: 0.0,
            fiber_g: 0.0,
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
    fn linked_reflection_filters_health_to_member() {
        let mut healths = vec![health_summary("alex"), health_summary("jordan")];
        filter_health_for_member(&mut healths, Some("JORDAN"));
        assert_eq!(healths.len(), 1);
        assert_eq!(healths[0].family_member_id, "jordan");

        let mut household = vec![health_summary("alex"), health_summary("jordan")];
        filter_health_for_member(&mut household, None);
        assert_eq!(household.len(), 2);
    }

    #[test]
    fn linked_reflections_use_member_distinct_paths() {
        let brain = PathBuf::from("/tmp/chotu-brain");
        let alex = reflection_file_path(&brain, "2026-09-07", Some("alex")).unwrap();
        let jordan = reflection_file_path(&brain, "2026-09-07", Some("jordan")).unwrap();
        let household = reflection_file_path(&brain, "2026-09-07", None).unwrap();

        assert_ne!(alex, jordan);
        assert!(alex.ends_with("Journal/2026/09/2026-09-07--alex.md"));
        assert!(jordan.ends_with("Journal/2026/09/2026-09-07--jordan.md"));
        assert!(household.ends_with("Journal/2026/09/2026-09-07.md"));
    }
}
