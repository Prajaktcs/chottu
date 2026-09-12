use anyhow::{Context, Result};
use chotu_common::{day_bounds_utc_in, ChotuLlm, HealthFamilySummary};
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

pub async fn get_daily_data(
    pool: &SqlitePool,
    date: &str,
    config: &chotu_common::AppConfig,
) -> Result<(Vec<SimpleTx>, Vec<HealthFamilySummary>)> {
    let (day_start, day_end) = day_bounds_utc_in(config.resolved_tz(), date)
        .with_context(|| format!("Invalid reflection date or timezone bounds: {date}"))?;
    let txs = sqlx::query_as::<_, SimpleTx>(
        r#"
        SELECT merchant, amount, category, currency
        FROM financial_ledger
        WHERE timestamp >= ? AND timestamp < ?
        "#,
    )
    .bind(day_start)
    .bind(day_end)
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
    use chrono::{DateTime, Utc};

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

    #[tokio::test]
    async fn daily_data_uses_configured_timezone_for_ledger_window() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE financial_ledger (
                timestamp DATETIME NOT NULL,
                merchant TEXT NOT NULL,
                amount REAL NOT NULL,
                category TEXT NOT NULL,
                currency TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("CREATE TABLE health_family_summary (date TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();

        for (merchant, timestamp) in [
            ("previous-local-day", "2026-09-07T03:30:00Z"),
            ("target-local-day", "2026-09-07T04:30:00Z"),
            ("next-local-day", "2026-09-08T04:30:00Z"),
        ] {
            let timestamp = DateTime::parse_from_rfc3339(timestamp)
                .unwrap()
                .with_timezone(&Utc);
            sqlx::query(
                "INSERT INTO financial_ledger
                 (timestamp, merchant, amount, category, currency)
                 VALUES (?, ?, 1.0, 'test', 'CAD')",
            )
            .bind(timestamp)
            .bind(merchant)
            .execute(&pool)
            .await
            .unwrap();
        }

        let mut config = chotu_common::AppConfig::default();
        config.timezone = Some("America/Toronto".to_string());
        let (txs, _) = get_daily_data(&pool, "2026-09-07", &config).await.unwrap();

        assert_eq!(
            txs.iter()
                .map(|tx| tx.merchant.as_str())
                .collect::<Vec<_>>(),
            vec!["target-local-day"]
        );
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
