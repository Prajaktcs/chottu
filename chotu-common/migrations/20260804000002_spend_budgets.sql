-- Telegram overrides for category spend budgets (win over config.yaml).
CREATE TABLE IF NOT EXISTS spend_budget_overrides (
    category TEXT PRIMARY KEY NOT NULL,
    limit_amount REAL NOT NULL,
    updated_at TEXT NOT NULL
);

-- Dedupe mid-month 80%/100% spend alerts per category per calendar month.
CREATE TABLE IF NOT EXISTS spend_budget_alerts (
    month TEXT NOT NULL,
    category TEXT NOT NULL,
    threshold INTEGER NOT NULL,
    sent_at TEXT NOT NULL,
    PRIMARY KEY (month, category, threshold)
);
