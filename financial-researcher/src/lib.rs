use std::path::PathBuf;
use anyhow::Context;
use rig_core::providers::gemini;
use rig_core::client::CompletionClient;
use rig_core::completion::Prompt;
use chotu_common::InvestmentPhilosophy;

#[derive(Debug, Clone)]
pub struct StockResearcher {
    client: gemini::Client,
}

impl StockResearcher {
    pub fn new(api_key: String) -> Self {
        Self {
            client: gemini::Client::new(&api_key).expect("Failed to initialize Gemini client"),
        }
    }

    /// Conducts research on stock candidates using the specified investment philosophy and targets.
    pub async fn perform_research(
        &self,
        targets: Option<&str>,
        philosophy: Option<&InvestmentPhilosophy>,
    ) -> Result<String, anyhow::Error> {
        let default_philosophy = InvestmentPhilosophy::default();
        let p = philosophy.unwrap_or(&default_philosophy);

        let focus_areas_str = p.focus_areas
            .iter()
            .enumerate()
            .map(|(i, area)| format!("{}. {}", i + 1, area))
            .collect::<Vec<String>>()
            .join(", ");

        let system_prompt = format!(
            "You are a professional equity research analyst specializing in {}. \
             Conduct deep analysis on 2-3 potential candidates, focusing on: {}. \
             Format the output in clean, structured Markdown suitable for reading on Telegram.",
            p.description,
            focus_areas_str
        );

        let user_prompt = match targets {
            Some(t) if !t.trim().is_empty() => format!(
                "Perform deep stock market research specifically for these requested companies/tickers: {}. \
                 Search for their tickers if names are given, and detail their business models, growth catalysts, \
                 key risks, and whether they fit the investment philosophy profile.",
                t
            ),
            _ => "Perform stock market research for potential candidates matching our investment philosophy. \
                  Detail 2-3 specific companies, their ticker symbols, business models, growth catalysts, and key risks.".to_string(),
        };

        let agent = self
            .client
            .agent("gemini-3.5-flash")
            .preamble(&system_prompt)
            .build();

        let response = agent
            .prompt(user_prompt)
            .await
            .map_err(|e| anyhow::anyhow!("Gemini LLM prompt failed: {}", e))?;

        Ok(response)
    }
}

/// Runs stock research using the provided researcher client, writes the output markdown file,
/// parses tickers, and logs the execution to the sqlite database.
pub async fn run_stock_research(
    pool: &sqlx::SqlitePool,
    researcher: &StockResearcher,
    philosophy: Option<&InvestmentPhilosophy>,
    targets: Option<&str>,
) -> Result<String, anyhow::Error> {
    let report = researcher
        .perform_research(targets, philosophy)
        .await?;

    // Save report to local disk (~/chotu_brain/Research/YYYY/MM/YYYY-MM-DD-stocks.md)
    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();
    let parts: Vec<&str> = date_str.split('-').collect();
    if parts.len() != 3 {
        return Err(anyhow::anyhow!(
            "Invalid date format for stock research save: {}",
            date_str
        ));
    }
    let year = parts[0];
    let month = parts[1];

    let brain_dir_str =
        std::env::var("CHOTU_BRAIN_DIR").unwrap_or_else(|_| "~/chotu_brain".to_string());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/user".to_string());
    let brain_path = PathBuf::from(brain_dir_str.replace("~", &home));
    let target_dir = brain_path.join("Research").join(year).join(month);
    tokio::fs::create_dir_all(&target_dir)
        .await
        .context("Failed to create target stock research directory")?;

    let file_path = target_dir.join(format!("{}-stocks.md", date_str));
    tokio::fs::write(&file_path, &report)
        .await
        .with_context(|| format!("Failed to write stock research file to {:?}", file_path))?;

    // Parse ticker names
    let tickers = extract_tickers(&report);
    let path_str = file_path.to_string_lossy().to_string();
    let log_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();

    sqlx::query(
        "INSERT INTO stock_research_log (id, timestamp, tickers_analyzed, summary_file_path) VALUES (?, ?, ?, ?)"
    )
    .bind(&log_id)
    .bind(now)
    .bind(&tickers)
    .bind(&path_str)
    .execute(pool)
    .await
    .context("Failed to insert stock research log to database")?;

    Ok(report)
}

pub fn extract_between<'a>(t: &'a str, start_delim: &str, end_delim: &str) -> Vec<&'a str> {
    let mut results = Vec::new();
    let mut temp = t;
    while let Some(start_idx) = temp.find(start_delim) {
        let start_pos = start_idx + start_delim.len();
        let remaining = &temp[start_pos..];
        if let Some(end_idx) = remaining.find(end_delim) {
            let inside = &remaining[..end_idx];
            results.push(inside);
            temp = &remaining[end_idx + end_delim.len()..];
        } else {
            break;
        }
    }
    results
}

pub fn extract_tickers(text: &str) -> String {
    use std::collections::BTreeSet;
    let mut tickers = BTreeSet::new();

    let is_valid_ticker = |w: &str| {
        let clean: String = w.chars().filter(|c| c.is_ascii_alphabetic()).collect();
        !clean.is_empty() && clean.len() <= 5 && clean.chars().all(|c| c.is_ascii_uppercase())
    };

    // 1. Process "Ticker:" and "ticker:" lines
    for line in text.lines() {
        let lower = line.to_lowercase();
        if let Some(idx) = lower.find("ticker:") {
            let rest = &line[idx + 7..];
            for word in rest.split(|c: char| {
                c.is_whitespace()
                    || c == ':'
                    || c == '$'
                    || c == '('
                    || c == ')'
                    || c == ','
                    || c == '*'
                    || c == '`'
                    || c == '.'
            }) {
                let clean: String = word.chars().filter(|c| c.is_ascii_alphabetic()).collect();
                if is_valid_ticker(&clean) {
                    tickers.insert(clean);
                }
            }
        }
    }

    // 2. Extracted inside parentheses/brackets/bold
    for inside in extract_between(text, "(", ")") {
        for part in inside.split(|c: char| c.is_whitespace() || c == ':') {
            let clean: String = part.chars().filter(|c| c.is_ascii_alphabetic()).collect();
            if is_valid_ticker(&clean) {
                tickers.insert(clean);
            }
        }
    }

    for inside in extract_between(text, "[", "]") {
        for part in inside.split(|c: char| c.is_whitespace() || c == ':') {
            let clean: String = part.chars().filter(|c| c.is_ascii_alphabetic()).collect();
            if is_valid_ticker(&clean) {
                tickers.insert(clean);
            }
        }
    }

    for inside in extract_between(text, "**", "**") {
        let clean: String = inside.chars().filter(|c| c.is_ascii_alphabetic()).collect();
        if is_valid_ticker(&clean) {
            tickers.insert(clean);
        }
    }

    // 3. Extracted words starting with $
    for word in
        text.split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '.' || c == ')')
    {
        if word.starts_with('$') {
            let clean: String = word.chars().filter(|c| c.is_ascii_alphabetic()).collect();
            if is_valid_ticker(&clean) {
                tickers.insert(clean);
            }
        }
    }

    let result: Vec<String> = tickers.into_iter().collect();
    result.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_tickers() {
        let sample = "\
### Stock Analysis Report
We present **Palantir Technologies** (Ticker: PLTR) as our first candidate.
The second candidate is $MELI, which has high margins.
Also check out Sea Limited (SE).
Some noise like Ticker: NOT_A_TICKER is ignored because it is too long.";
        let result = extract_tickers(sample);
        assert!(result.contains("PLTR"));
        assert!(result.contains("MELI"));
        assert!(result.contains("SE"));
        assert!(!result.contains("NOT_A_TICKER"));
    }
}
