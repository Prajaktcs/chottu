use anyhow::{Context, Result};
use chotu_common::{ChotuLlm, HealthFamilySummary};
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
    // Query financials
    let txs = sqlx::query_as::<_, SimpleTx>(
        r#"
        SELECT merchant, amount, category, currency
        FROM financial_ledger
        WHERE date(timestamp) = ?
        "#,
    )
    .bind(date)
    .fetch_all(pool)
    .await
    .context("Failed to query daily financials")?;

    // Query health summaries
    let db_healths = sqlx::query_as::<_, HealthFamilySummary>(
        r#"
        SELECT *
        FROM health_family_summary
        WHERE date = ?
        "#
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

    // Construct target path: Journal/YYYY/MM/YYYY-MM-DD.md
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return Err(anyhow::anyhow!(
            "Invalid date format for reflection save: {}",
            date
        ));
    }
    let year = parts[0];
    let month = parts[1];

    let target_dir = brain_path.join("Journal").join(year).join(month);
    tokio::fs::create_dir_all(&target_dir)
        .await
        .context("Failed to create target reflection journal directory")?;

    let file_path = target_dir.join(format!("{}.md", date));

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
}
