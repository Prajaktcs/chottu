-- Expand Signal-only delivery state into provider-scoped chat state.
-- Legacy tables stay in place for one compatibility release and are dual-written by Signal.

CREATE TABLE task_chat_messages (
    task_id TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('signal', 'telegram')),
    conversation_kind TEXT NOT NULL CHECK (conversation_kind IN ('direct', 'group')),
    conversation_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    PRIMARY KEY (provider, conversation_kind, conversation_id, message_id)
);

CREATE INDEX idx_task_chat_messages_task_id ON task_chat_messages(task_id);

INSERT INTO task_chat_messages (
    task_id, provider, conversation_kind, conversation_id, message_id
)
SELECT task_id, 'signal', recipient_kind, recipient_id, CAST(message_timestamp AS TEXT)
FROM task_signal_messages;

CREATE TABLE task_chat_due_reminder_deliveries (
    task_id TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('signal', 'telegram')),
    conversation_kind TEXT NOT NULL CHECK (conversation_kind IN ('direct', 'group')),
    conversation_id TEXT NOT NULL,
    PRIMARY KEY (task_id, provider, conversation_kind, conversation_id)
);

INSERT INTO task_chat_due_reminder_deliveries (
    task_id, provider, conversation_kind, conversation_id
)
SELECT task_id, 'signal', recipient_kind, recipient_id
FROM task_due_reminder_deliveries;

CREATE TABLE scheduled_chat_deliveries (
    job TEXT NOT NULL CHECK (job IN ('morning_brief', 'portfolio', 'reflection')),
    local_date TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('signal', 'telegram')),
    conversation_kind TEXT NOT NULL CHECK (conversation_kind IN ('direct', 'group')),
    conversation_id TEXT NOT NULL,
    delivered_at TEXT,
    retry_after_epoch INTEGER,
    PRIMARY KEY (job, local_date, provider, conversation_kind, conversation_id)
);

CREATE INDEX scheduled_chat_deliveries_pending
    ON scheduled_chat_deliveries(job, local_date, delivered_at, retry_after_epoch);

INSERT INTO scheduled_chat_deliveries (
    job, local_date, provider, conversation_kind, conversation_id,
    delivered_at, retry_after_epoch
)
SELECT job, local_date, 'signal', recipient_kind, recipient_id,
       delivered_at, retry_after_epoch
FROM scheduled_signal_deliveries;

CREATE TABLE email_task_chat_deliveries (
    task_id TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('signal', 'telegram')),
    target_kind TEXT NOT NULL CHECK (target_kind IN ('direct', 'group', 'member')),
    target_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'sending', 'delivered')),
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    lease_expires_at INTEGER,
    last_error TEXT,
    message_id TEXT,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    delivered_at DATETIME,
    PRIMARY KEY (task_id, provider, target_kind, target_id)
);

CREATE INDEX email_task_chat_deliveries_pending
    ON email_task_chat_deliveries (provider, state, next_attempt_at);

INSERT INTO email_task_chat_deliveries (
    task_id, provider, target_kind, target_id, state, attempts,
    next_attempt_at, lease_expires_at, last_error, message_id, created_at, delivered_at
)
SELECT task_id, 'signal', target_kind, target_id, state, attempts,
       next_attempt_at, lease_expires_at, last_error, CAST(message_timestamp AS TEXT),
       created_at, delivered_at
FROM email_task_signal_deliveries;
