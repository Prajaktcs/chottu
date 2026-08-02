use std::path::PathBuf;
use anyhow::Context;
use rig_core::providers::gemini;
use rig_core::client::CompletionClient;
use rig_core::completion::Prompt;
use chotu_common::{InvestmentPhilosophy, TargetAllocation, FinancialLedgerEntry, AppConfig};

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

fn match_transaction(merchant: &str, category: &str, ticker: &str) -> bool {
    let merchant_upper = merchant.to_uppercase();
    let category_upper = category.to_uppercase();
    let ticker_upper = ticker.to_uppercase();
    
    // Smart fallbacks for general investment buckets
    if ticker_upper == "MICRO-CAP PICKS" {
        return merchant_upper.contains("QUESTRADE") 
            || merchant_upper.contains("QUESTTRADE")
            || merchant_upper.contains("WEALTHSIMPLE")
            || merchant_upper.contains("ROBINHOOD")
            || category_upper == "INVESTMENT";
    }

    if let Some(idx) = merchant_upper.find(&ticker_upper) {
        let before_ok = idx == 0 || !merchant_upper.chars().nth(idx - 1).unwrap_or(' ').is_alphanumeric();
        let after_ok = idx + ticker_upper.len() == merchant_upper.len()
            || !merchant_upper.chars().nth(idx + ticker_upper.len()).unwrap_or(' ').is_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
    }
    
    false
}

pub fn check_allocation_status(
    target_month: &str,
    allocation: &TargetAllocation,
    entries: &[FinancialLedgerEntry],
    config: &AppConfig,
    rates: &std::collections::HashMap<String, f64>,
) -> String {
    let mut msg = String::new();
    msg.push_str(&format!("🎯 *Savings & Target Allocation Tracking: {}*\n\n", target_month));

    let mut total_actual_buys = 0.0;
    
    for bucket in &allocation.buckets {
        let mut actual_bucket_buy = 0.0;
        let mut holdings_lines = Vec::new();
        
        for holding in &bucket.holdings {
            let mut actual_holding_buy = 0.0;
            for entry in entries {
                // We only count spend/buy transactions (which are debits/negative amounts)
                if entry.amount < 0.0 && match_transaction(&entry.merchant, &entry.category, &holding.ticker) {
                    let converted = config.convert_to_base(entry.amount.abs(), &entry.currency, rates);
                    actual_holding_buy += converted;
                }
            }
            actual_bucket_buy += actual_holding_buy;
            total_actual_buys += actual_holding_buy;
            
            let holding_percent = if holding.amount > 0.0 {
                (actual_holding_buy / holding.amount) * 100.0
            } else {
                0.0
            };
            
            let status_icon = if actual_holding_buy >= holding.amount {
                "✅"
            } else if actual_holding_buy > 0.0 {
                "⚠️"
            } else {
                "❌"
            };
            
            holdings_lines.push(format!(
                "  - *{}*: ${:.2} / ${:.2} ({:.1}% - {})",
                holding.ticker, actual_holding_buy, holding.amount, holding_percent, status_icon
            ));
        }
        
        let bucket_percent = if bucket.monthly_buy > 0.0 {
            (actual_bucket_buy / bucket.monthly_buy) * 100.0
        } else {
            0.0
        };
        
        let bucket_status_icon = if actual_bucket_buy >= bucket.monthly_buy {
            "✅"
        } else if actual_bucket_buy > 0.0 {
            "⚠️"
        } else {
            "❌"
        };
        
        msg.push_str(&format!(
            "• *{}* (Target: ${:.2} | Actual: ${:.2} | {:.1}% - {})\n",
            bucket.name, bucket.monthly_buy, actual_bucket_buy, bucket_percent, bucket_status_icon
        ));
        for line in holdings_lines {
            msg.push_str(&line);
            msg.push_str("\n");
        }
        msg.push_str("\n");
    }
    
    let overall_percent = if allocation.monthly_budget > 0.0 {
        (total_actual_buys / allocation.monthly_budget) * 100.0
    } else {
        0.0
    };
    
    let overall_status = if total_actual_buys >= allocation.monthly_budget {
        "On Track ✅"
    } else if total_actual_buys > 0.0 {
        "Partially Funded ⚠️"
    } else {
        "Not Funded ❌"
    };
    
    msg.push_str("━━━━━━━━━━━━━━━━━━━━━━━━\n");
    msg.push_str(&format!(
        "✨ *Overall Savings Budget:* ${:.2} / ${:.2} ({:.1}% - {})\n",
        total_actual_buys, allocation.monthly_budget, overall_percent, overall_status
    ));
    
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use chotu_common::{AllocationBucket, BucketHolding};

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

    #[test]
    fn test_check_allocation_status() {
        let allocation = TargetAllocation {
            monthly_budget: 1000.0,
            buckets: vec![
                AllocationBucket {
                    name: "Core Equities".to_string(),
                    weight_percent: 100.0,
                    monthly_buy: 1000.0,
                    holdings: vec![
                        BucketHolding { ticker: "VFV".to_string(), amount: 600.0 },
                        BucketHolding { ticker: "QQC".to_string(), amount: 400.0 },
                    ],
                }
            ],
        };
        
        let entries = vec![
            FinancialLedgerEntry {
                id: "1".to_string(),
                timestamp: chrono::Utc::now(),
                amount: -600.0,
                currency: "USD".to_string(),
                institution: "Questrade".to_string(),
                merchant: "VFV - Vanguard S&P 500 ETF: Bought shares".to_string(),
                category: "Uncategorized".to_string(),
                source_type: "BATCH_DROP".to_string(),
            },
            FinancialLedgerEntry {
                id: "2".to_string(),
                timestamp: chrono::Utc::now(),
                amount: -150.0,
                currency: "USD".to_string(),
                institution: "Questrade".to_string(),
                merchant: "QQC Bought".to_string(),
                category: "Uncategorized".to_string(),
                source_type: "BATCH_DROP".to_string(),
            },
        ];
        
        let config = AppConfig::default();
        let rates = std::collections::HashMap::new();
        let report = check_allocation_status("2026-06", &allocation, &entries, &config, &rates);
        assert!(report.contains("VFV"));
        assert!(report.contains("$600.00 / $600.00"));
        assert!(report.contains("QQC"));
        assert!(report.contains("$150.00 / $400.00"));
        assert!(report.contains("Core Equities"));
        assert!(report.contains("Overall Savings Budget"));
    }
}
