-- Match overlapping CSV exports without collapsing repeated transactions within a source.
-- Receipt and email ledger records do not participate in CSV deduplication.
CREATE TABLE IF NOT EXISTS csv_import_records (
    ledger_id TEXT PRIMARY KEY, -- financial_ledger.id (logical reference)
    has_time INTEGER NOT NULL,
    date_quality INTEGER NOT NULL,
    merchant_quality INTEGER NOT NULL,
    category_quality INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS csv_import_keys (
    match_key TEXT NOT NULL,
    ledger_id TEXT NOT NULL, -- csv_import_records.ledger_id (logical reference)
    PRIMARY KEY (match_key, ledger_id)
);
CREATE INDEX IF NOT EXISTS csv_import_keys_ledger ON csv_import_keys(ledger_id);
