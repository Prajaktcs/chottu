//! Household category spend budgets: progress, Telegram overrides, and alert dedupe.

use std::collections::HashMap;

use chrono::Local;
use sqlx::SqlitePool;

use crate::agenda::escape_md;
use crate::{fetch_exchange_rates, AppConfig};

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

    let overrides: Vec<(String, f64)> = sqlx::query_as(
        "SELECT category, limit_amount FROM spend_budget_overrides",
    )
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
pub async fn clear_budget_override(
    pool: &SqlitePool,
    category: &str,
) -> Result<bool, sqlx::Error> {
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
}

async fn category_spend_for_month(
    pool: &SqlitePool,
    config: &AppConfig,
    month: &str,
    rates: &HashMap<String, f64>,
) -> Result<HashMap<String, f64>, sqlx::Error> {
    let entries: Vec<LedgerSpendRow> = sqlx::query_as(
        "SELECT amount, currency, category \
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
        let amt = config
            .convert_to_base(entry.amount, &entry.currency, rates)
            .abs();
        *totals.entry(norm).or_insert(0.0) += amt;
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
pub fn format_budget_progress_markdown(
    month: &str,
    base: &str,
    rows: &[BudgetProgress],
) -> String {
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

    #[test]
    fn format_progress_empty() {
        let md = format_budget_progress_markdown("2026-08", "CAD", &[]);
        assert!(md.contains("No category budgets"));
    }

    #[test]
    fn format_progress_flags() {
        let rows = vec![
            BudgetProgress {
                category: "Food".into(),
                spent: 710.0,
                limit: 800.0,
                pct: 88.75,
            },
            BudgetProgress {
                category: "Shopping".into(),
                spent: 120.0,
                limit: 400.0,
                pct: 30.0,
            },
        ];
        let md = format_budget_progress_markdown("2026-08", "CAD", &rows);
        assert!(md.contains("watch"));
        assert!(md.contains("Food"));
        assert!(md.contains("Shopping"));
    }

    #[test]
    fn format_alert_messages() {
        let watch = BudgetAlert {
            category: "Food".into(),
            spent: 710.0,
            limit: 800.0,
            pct: 89.0,
            threshold: 80,
        };
        let md = watch.format_markdown("CAD");
        assert!(md.contains("left"));
        assert!(md.contains("Food"));

        let over = BudgetAlert {
            category: "Food".into(),
            spent: 850.0,
            limit: 800.0,
            pct: 106.0,
            threshold: 100,
        };
        let md = over.format_markdown("CAD");
        assert!(md.contains("over by"));
    }
}
