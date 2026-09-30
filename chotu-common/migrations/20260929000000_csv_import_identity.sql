-- Match overlapping CSV exports without collapsing repeated transactions within a source.
-- Receipt and email ledger records do not participate in CSV deduplication.
CREATE TABLE IF NOT EXISTS csv_import_records (
    ledger_id TEXT PRIMARY KEY REFERENCES financial_ledger(id) ON DELETE CASCADE,
    has_time INTEGER NOT NULL,
    merchant_quality INTEGER NOT NULL,
    category_quality INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS csv_import_keys (
    match_key TEXT NOT NULL,
    ledger_id TEXT NOT NULL REFERENCES csv_import_records(ledger_id) ON DELETE CASCADE,
    PRIMARY KEY (match_key, ledger_id)
);
CREATE INDEX IF NOT EXISTS csv_import_keys_ledger ON csv_import_keys(ledger_id);
