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
            logs_summary.push_str(&format!(
                "- Member: {}\n  * Nutrition: {} kcal ingested (Protein: {}g, Carbs: {}g, Fats: {}g)\n  * Activity: {} steps, {} active calories burned\n  * Sleep: {} hrs\n  * Energy Level: {}\n",
                h.family_member_id,
                h.total_calories_ingested,
                h.protein_grams,
                h.carbs_grams,
                h.fats_grams,
                h.step_count,
                h.active_calories_burned,
                h.sleep_hours.unwrap_or(0.0),
                h.perceived_energy.map(|e| e.to_string()).unwrap_or("N/A".to_string())
            ));
        }
    }

    let system_prompt = "You are Chotu's Evening Reflection Engine, an AI journaling assistant for the family. \
You must construct a highly personalized reflection prompt (1-3 sentences) based ONLY on the provided financial and family health logs for today. \
Avoid referencing general, non-provided, or generic advice. \
Acknowledge specific achievements or note specific trends (e.g., high step count, late night meal, zero spend day, category spend) only if they are present in the logs. \
End with one or two open-ended questions asking the user to reflect on their day. \
Do not include any other text, reasoning, frontmatter, or commentary. Only return the final reflection prompt.";

    let user_prompt = format!("Date: {}\nLogs:\n{}", date, logs_summary);

    let prompt = llm
        .generate_prompt(system_prompt, &user_prompt)
        .await
        .map_err(|e| anyhow::anyhow!("LLM error: {:?}", e))?;

    // If DeepSeek-R1 returned thought blocks, strip them out (anything between <think> and </think>)
    let cleaned_prompt = strip_think_blocks(&prompt);

    Ok(cleaned_prompt)
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

    // Escape prompt text for YAML double quotes
    let escaped_prompt = prompt.replace('\n', " ").replace('"', "\\\"");
    content.push_str(&format!("prompt: \"{}\"\n", escaped_prompt));

    content.push_str("financials:\n");
    let total_spent: f64 = txs.iter().map(|tx| tx.amount).sum();
    content.push_str(&format!("  total_spent: {:.2}\n", total_spent));
    content.push_str("  transactions:\n");
    for tx in txs {
        content.push_str(&format!(
            "    - merchant: \"{}\"\n",
            tx.merchant.replace('"', "\\\"")
        ));
        content.push_str(&format!("      amount: {:.2}\n", tx.amount));
        content.push_str(&format!(
            "      category: \"{}\"\n",
            tx.category.replace('"', "\\\"")
        ));
        content.push_str(&format!("      currency: \"{}\"\n", tx.currency));
    }

    content.push_str("health:\n");
    for h in healths {
        content.push_str(&format!("  {}:\n", h.family_member_id));
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
        content.push_str(&format!("    sleep: {}\n", h.sleep_hours.unwrap_or(0.0)));
        content.push_str(&format!(
            "    perceived_energy: {}\n",
            h.perceived_energy.unwrap_or(0)
        ));
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
}
