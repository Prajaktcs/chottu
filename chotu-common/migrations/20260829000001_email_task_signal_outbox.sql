-- Durable per-target delivery state for Signal reminders inferred from email.
-- A task and its delivery rows are inserted in one transaction by the streamer.
CREATE TABLE email_task_signal_deliveries (
    task_id TEXT NOT NULL,
    target_kind TEXT NOT NULL CHECK (target_kind IN ('direct', 'group', 'member')),
    target_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'sending', 'delivered')),
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    lease_expires_at INTEGER,
    last_error TEXT,
    message_timestamp INTEGER,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    delivered_at DATETIME,
    PRIMARY KEY (task_id, target_kind, target_id)
);

CREATE INDEX idx_email_task_signal_deliveries_pending
    ON email_task_signal_deliveries (state, next_attempt_at);
