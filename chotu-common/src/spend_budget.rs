//! Household category spend budgets: progress, Telegram overrides, and alert dedupe.

use std::collections::HashMap;

use chrono::Local;
use sqlx::SqlitePool;

use crate::agenda::escape_md;
use crate::{expense_contribution, fetch_exchange_rates, AppConfig};

pub const BUDGET_THRESHOLDS: [i32; 2] = [80, 100];

#[derive(Debug, Clone, PartialEq)]
pub struct BudgetProgress {
    pub category: String,
    pub spent: f64,
    pub limit: f64,
    pub pct: f64,
}

impl BudgetProgress {
    pub fn remaining(&self) -> f64 {
        self.limit - self.spent
    }

    pub fn over_by(&self) -> f64 {
        (self.spent - self.limit).max(0.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BudgetAlert {
    pub category: String,
    pub spent: f64,
    pub limit: f64,
    pub pct: f64,
    pub threshold: i32,
}

impl BudgetAlert {
    fn over_by_amount(&self) -> f64 {
        (self.spent - self.limit).max(0.0)
    }

    pub fn format_markdown(&self, base: &str) -> String {
        let category = escape_md(&self.category);
        if self.threshold >= 100 {
            if self.spent > self.limit {
                format!(
                    "🚨 *Spend alert · {}*\n${:.0} / ${:.0} ({:.0}%) — over by ${:.0} {}",
                    category,
                    self.spent,
                    self.limit,
                    self.pct,
                    self.over_by_amount(),
                    base
                )
            } else {
                format!(
                    "🚨 *Spend alert · {}*\n${:.0} / ${:.0} ({:.0}%) — at limit ({})",
                    category, self.spent, self.limit, self.pct, base
                )
            }
        } else {
            let left = (self.limit - self.spent).max(0.0);
            format!(
                "⚠️ *Spend alert · {}*\n${:.0} / ${:.0} ({:.0}%) — ${:.0} {} left",
                category, self.spent, self.limit, self.pct, left, base
            )
        }
    }
}

/// Canonicalize category for storage/lookup (trim + lowercase).
pub fn normalize_category(category: &str) -> String {
    category.trim().to_lowercase()
}

/// Display form: Title Case each word.
pub fn display_category(category: &str) -> String {
    let trimmed = category.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    trimmed
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => {
                    let mut s = c.to_uppercase().collect::<String>();
                    s.push_str(&chars.as_str().to_lowercase());
                    s
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Merge YAML budgets with SQLite overrides (overrides win). Keys are display names.
pub async fn effective_budgets(
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<HashMap<String, f64>, sqlx::Error> {
    let mut by_norm: HashMap<String, (String, f64)> = HashMap::new();

    if let Some(ref budgets) = config.spend_budgets {
        for (cat, limit) in &budgets.categories {
            if *limit <= 0.0 {
                continue;
            }
            let norm = normalize_category(cat);
            if norm.is_empty() || norm == "income" {
                continue;
            }
            by_norm.insert(norm, (display_category(cat), *limit));
        }
    }

    let overrides: Vec<(String, f64)> =
        sqlx::query_as("SELECT category, limit_amount FROM spend_budget_overrides")
            .fetch_all(pool)
            .await?;

    for (cat, limit) in overrides {
        if limit <= 0.0 {
            continue;
        }
        let norm = normalize_category(&cat);
        if norm.is_empty() || norm == "income" {
            continue;
        }
        by_norm.insert(norm, (display_category(&cat), limit));
    }

    Ok(by_norm
        .into_values()
        .map(|(name, limit)| (name, limit))
        .collect())
}

/// Upsert a Telegram override for a category monthly limit.
pub async fn set_budget_override(
    pool: &SqlitePool,
    category: &str,
    limit: f64,
) -> Result<(), sqlx::Error> {
    let display = display_category(category);
    let norm = normalize_category(category);
    let now = Local::now().to_rfc3339();
    sqlx::query("DELETE FROM spend_budget_overrides WHERE lower(category) = ?")
        .bind(&norm)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO spend_budget_overrides (category, limit_amount, updated_at) \
         VALUES (?, ?, ?)",
    )
    .bind(&display)
    .bind(limit)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove a Telegram override (falls back to YAML if present).
pub async fn clear_budget_override(pool: &SqlitePool, category: &str) -> Result<bool, sqlx::Error> {
    let norm = normalize_category(category);
    let result = sqlx::query("DELETE FROM spend_budget_overrides WHERE lower(category) = ?")
        .bind(&norm)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct LedgerSpendRow {
    amount: f64,
    currency: String,
    category: String,
    source_type: String,
}

async fn category_spend_for_month(
    pool: &SqlitePool,
    config: &AppConfig,
    month: &str,
    rates: &HashMap<String, f64>,
) -> Result<HashMap<String, f64>, sqlx::Error> {
    let entries: Vec<LedgerSpendRow> = sqlx::query_as(
        "SELECT amount, currency, category, source_type \
         FROM financial_ledger \
         WHERE strftime('%Y-%m', timestamp) = ?",
    )
    .bind(month)
    .fetch_all(pool)
    .await?;

    let mut totals: HashMap<String, f64> = HashMap::new();
    for entry in &entries {
        let norm = normalize_category(&entry.category);
        if norm.is_empty() || norm == "income" {
            continue;
        }
        let amt = config.convert_to_base(
            expense_contribution(entry.amount, &entry.source_type, &entry.category),
            &entry.currency,
            rates,
        );
        if amt != 0.0 {
            *totals.entry(norm).or_insert(0.0) += amt;
        }
    }
    Ok(totals)
}

/// Compute progress for all effective budgets in `month` (YYYY-MM).
pub async fn compute_budget_progress(
    pool: &SqlitePool,
    config: &AppConfig,
    month: &str,
) -> Result<Vec<BudgetProgress>, sqlx::Error> {
    let budgets = effective_budgets(pool, config).await?;
    if budgets.is_empty() {
        return Ok(Vec::new());
    }

    let base = config.currency();
    let rates = fetch_exchange_rates(base).await;
    let spend = category_spend_for_month(pool, config, month, &rates).await?;

    let mut rows: Vec<BudgetProgress> = budgets
        .into_iter()
        .map(|(category, limit)| {
            let spent = spend
                .get(&normalize_category(&category))
                .copied()
                .unwrap_or(0.0);
            let pct = if limit > 0.0 {
                (spent / limit) * 100.0
            } else {
                0.0
            };
            BudgetProgress {
                category,
                spent,
                limit,
                pct,
            }
        })
        .collect();

    rows.sort_by(|a, b| {
        b.pct
            .partial_cmp(&a.pct)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.category.cmp(&b.category))
    });
    Ok(rows)
}

/// Format pull-surface Markdown for `/budget` and `/monthly` append.
pub fn format_budget_progress_markdown(month: &str, base: &str, rows: &[BudgetProgress]) -> String {
    if rows.is_empty() {
        return format!(
            "📊 *Budgets · {}* ({})\n\n_No category budgets configured. \
             Add `spend_budgets` in config.yaml or `/budget set Food 800`._\n",
            month, base
        );
    }

    let mut msg = format!("📊 *Budgets · {}* ({})\n\n", month, base);
    for row in rows {
        let flag = if row.pct >= 100.0 {
            " ⚠️ over"
        } else if row.pct >= 80.0 {
            " ← watch"
        } else {
            ""
        };
        msg.push_str(&format!(
            "• *{}*: ${:.0} / ${:.0} ({:.0}%){}\n",
            escape_md(&row.category),
            row.spent,
            row.limit,
            row.pct,
            flag
        ));
    }
    msg
}

async fn alert_already_sent(
    pool: &SqlitePool,
    month: &str,
    category: &str,
    threshold: i32,
) -> Result<bool, sqlx::Error> {
    let norm = normalize_category(category);
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM spend_budget_alerts \
         WHERE month = ? AND lower(category) = ? AND threshold = ?",
    )
    .bind(month)
    .bind(&norm)
    .bind(threshold)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

async fn record_alert_sent(
    pool: &SqlitePool,
    month: &str,
    category: &str,
    threshold: i32,
) -> Result<(), sqlx::Error> {
    let now = Local::now().to_rfc3339();
    let display = display_category(category);
    sqlx::query(
        "INSERT OR IGNORE INTO spend_budget_alerts (month, category, threshold, sent_at) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(month)
    .bind(&display)
    .bind(threshold)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Find newly crossed 80/100 thresholds that have not been alerted yet (does not mark sent).
pub async fn pending_budget_alerts(
    pool: &SqlitePool,
    config: &AppConfig,
    month: &str,
) -> Result<Vec<BudgetAlert>, sqlx::Error> {
    let rows = compute_budget_progress(pool, config, month).await?;
    let mut alerts = Vec::new();

    for row in rows {
        for &threshold in &BUDGET_THRESHOLDS {
            if row.pct + f64::EPSILON < threshold as f64 {
                continue;
            }
            if alert_already_sent(pool, month, &row.category, threshold).await? {
                continue;
            }
            alerts.push(BudgetAlert {
                category: row.category.clone(),
                spent: row.spent,
                limit: row.limit,
                pct: row.pct,
                threshold,
            });
        }
    }

    alerts.sort_by(|a, b| {
        b.threshold
            .cmp(&a.threshold)
            .then_with(|| a.category.cmp(&b.category))
    });
    Ok(alerts)
}

/// Mark a threshold alert as sent for the month (dedupe).
pub async fn mark_budget_alert_sent(
    pool: &SqlitePool,
    month: &str,
    category: &str,
    threshold: i32,
) -> Result<(), sqlx::Error> {
    record_alert_sent(pool, month, category, threshold).await
}

/// Current local calendar month as YYYY-MM.
pub fn current_budget_month() -> String {
    Local::now().format("%Y-%m").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_and_display() {
        assert_eq!(normalize_category("  Food "), "food");
        assert_eq!(display_category("food"), "Food");
        assert_eq!(display_category("ENTERTAINMENT"), "Entertainment");
    }

    #[tokio::test]
    async fn no_configured_budgets_produce_no_progress_or_alerts() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE spend_budget_overrides (category TEXT, limit_amount REAL);
             CREATE TABLE financial_ledger (timestamp TEXT, amount REAL, currency TEXT, category TEXT, source_type TEXT);
             CREATE TABLE spend_budget_alerts (month TEXT, category TEXT, threshold INTEGER, sent_at TEXT)",
        )
            .execute(&pool)
            .await
            .unwrap();
        let config = AppConfig::default();
        let progress = compute_budget_progress(&pool, &config, "2026-08")
            .await
            .unwrap();
        assert!(progress.is_empty());
        let display = format_budget_progress_markdown("2026-08", "CAD", &progress);
        assert!(display.contains("/budget set"));
        assert!(pending_budget_alerts(&pool, &config, "2026-08")
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn progress_status_changes_at_watch_and_limit_boundaries() {
        for (pct, watch, over) in [
            (79.99, false, false),
            (80.0, true, false),
            (99.99, true, false),
            (100.0, false, true),
            (100.01, false, true),
        ] {
            let row = BudgetProgress {
                category: "Food".into(),
                spent: pct,
                limit: 100.0,
                pct,
            };
            let display = format_budget_progress_markdown("2026-08", "CAD", &[row]);
            assert_eq!(display.contains("watch"), watch, "pct={pct}");
            assert_eq!(display.contains("over"), over, "pct={pct}");
        }
    }

    #[test]
    fn alert_display_reports_remaining_and_overage_amounts_at_boundaries() {
        for (spent, threshold, expected_amounts) in [
            (640.0, 80, vec![640, 800, 160]),
            (800.0, 100, vec![800, 800]),
            (850.0, 100, vec![850, 800, 50]),
        ] {
            let alert = BudgetAlert {
                category: "Food".into(),
                spent,
                limit: 800.0,
                pct: spent / 800.0 * 100.0,
                threshold,
            };
            let display = alert.format_markdown("CAD");
            let amounts: Vec<u32> = display
                .split('$')
                .skip(1)
                .map(|part| {
                    part.split(|c: char| !c.is_ascii_digit())
                        .next()
                        .unwrap()
                        .parse()
                        .unwrap()
                })
                .collect();
            assert_eq!(amounts, expected_amounts, "spent={spent}");
        }
    }

    #[tokio::test]
    async fn category_spend_nets_csv_refunds_and_preserves_legacy_expenses() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE financial_ledger (
                timestamp TEXT, amount REAL, currency TEXT, category TEXT, source_type TEXT
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        let config = AppConfig::default();
        for (amount, category, source, timestamp) in [
            (-120.0, "Food", "CSV_IMPORT", "2026-08-01"),
            (25.0, "Food", "CSV_IMPORT", "2026-08-02"),
            (30.0, "Food", "EMAIL_STREAM", "2026-08-03"),
            (-10.0, "Food", "BATCH_DROP", "2026-08-04"),
            (-500.0, "Transfer", "CSV_IMPORT", "2026-08-05"),
            (-600.0, "Investment", "CSV_IMPORT", "2026-08-06"),
            (1000.0, "Income", "CSV_IMPORT", "2026-08-07"),
            (-900.0, "Food", "CSV_IMPORT", "2026-07-31"),
        ] {
            sqlx::query(
                "INSERT INTO financial_ledger
                 (timestamp, amount, currency, category, source_type) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(timestamp)
            .bind(amount)
            .bind(config.currency())
            .bind(category)
            .bind(source)
            .execute(&pool)
            .await
            .unwrap();
        }
        let totals = category_spend_for_month(&pool, &config, "2026-08", &HashMap::new())
            .await
            .unwrap();
        assert_eq!(totals["food"], 135.0);
        assert_eq!(totals.values().sum::<f64>(), 135.0);
        assert!(!totals.contains_key("income"));
        assert!(!totals.contains_key("transfer"));
        assert!(!totals.contains_key("investment"));
    }
}
