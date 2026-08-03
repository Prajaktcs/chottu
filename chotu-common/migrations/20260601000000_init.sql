-- Initial database schema setup

CREATE TABLE IF NOT EXISTS financial_ledger (
    id TEXT PRIMARY KEY,
    timestamp DATETIME NOT NULL,
    amount REAL NOT NULL,
    currency TEXT NOT NULL,
    institution TEXT NOT NULL,
    merchant TEXT NOT NULL,
    category TEXT NOT NULL,
    source_type TEXT NOT NULL     -- 'EMAIL_STREAM' or 'BATCH_DROP'
);

CREATE TABLE IF NOT EXISTS portfolio_holdings (
    ticker TEXT PRIMARY KEY,
    shares_owned REAL NOT NULL,
    average_cost REAL NOT NULL,
    last_updated DATETIME NOT NULL
);

-- Re-designed Multi-User Family Health Ledger
CREATE TABLE IF NOT EXISTS health_family_summary (
    date TEXT NOT NULL,                  -- YYYY-MM-DD
    family_member_id TEXT NOT NULL,      -- 'praj', 'wife', 'kid'
    total_calories_ingested INTEGER DEFAULT 0,
    protein_grams REAL DEFAULT 0.0,
    carbs_grams REAL DEFAULT 0.0,
    fats_grams REAL DEFAULT 0.0,
    step_count INTEGER DEFAULT 0,
    active_calories_burned INTEGER DEFAULT 0,
    sleep_hours REAL,
    perceived_energy INTEGER,            -- (Adults only)
    PRIMARY KEY (date, family_member_id) -- Prevents duplicate entries per person per day
);

-- Raw Food and Health Log Audit Table for Tracking Descriptions
CREATE TABLE IF NOT EXISTS food_log (
    id TEXT PRIMARY KEY,               -- UUID
    timestamp DATETIME NOT NULL,
    family_member_id TEXT NOT NULL,
    raw_text_description TEXT NOT NULL,
    estimated_calories INTEGER NOT NULL
);

-- Evaluation Metric Audit Log for Tracking System Prompts over time
CREATE TABLE IF NOT EXISTS evaluation_log (
    eval_id TEXT PRIMARY KEY,
    test_timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
    prompt_version TEXT NOT NULL,
    model_name TEXT NOT NULL,
    triage_accuracy REAL NOT NULL,
    extraction_faithfulness REAL NOT NULL
);
