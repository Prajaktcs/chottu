-- Allow provider-permanent delivery failures to stop retrying while retaining audit state.
CREATE TABLE email_task_chat_deliveries_new (
    task_id TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('signal', 'telegram')),
    target_kind TEXT NOT NULL CHECK (target_kind IN ('direct', 'group', 'member')),
    target_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'sending', 'delivered', 'failed')),
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    lease_expires_at INTEGER,
    last_error TEXT,
    message_id TEXT,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    delivered_at DATETIME,
    PRIMARY KEY (task_id, provider, target_kind, target_id)
);

INSERT INTO email_task_chat_deliveries_new (
    task_id, provider, target_kind, target_id, state, attempts,
    next_attempt_at, lease_expires_at, last_error, message_id, created_at, delivered_at
)
SELECT task_id, provider, target_kind, target_id, state, attempts,
       next_attempt_at, lease_expires_at, last_error, message_id, created_at, delivered_at
FROM email_task_chat_deliveries;

DROP TABLE email_task_chat_deliveries;
ALTER TABLE email_task_chat_deliveries_new RENAME TO email_task_chat_deliveries;

CREATE INDEX email_task_chat_deliveries_pending
    ON email_task_chat_deliveries (provider, state, next_attempt_at);
