-- Durable per-recipient state for scheduled Signal jobs. Pending rows survive
-- coordinator restarts; delivered rows prevent duplicate fan-out retries.
CREATE TABLE scheduled_signal_deliveries (
    job TEXT NOT NULL CHECK (job IN ('morning_brief', 'portfolio', 'reflection')),
    local_date TEXT NOT NULL,
    recipient_kind TEXT NOT NULL CHECK (recipient_kind IN ('direct', 'group')),
    recipient_id TEXT NOT NULL,
    delivered_at TEXT,
    retry_after_epoch INTEGER,
    PRIMARY KEY (job, local_date, recipient_kind, recipient_id)
);

CREATE INDEX scheduled_signal_deliveries_pending
    ON scheduled_signal_deliveries(job, local_date, delivered_at, retry_after_epoch);
