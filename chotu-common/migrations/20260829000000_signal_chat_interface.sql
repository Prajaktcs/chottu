-- Replace Telegram reminder correlation with Signal recipient/timestamp mappings.
ALTER TABLE tasks DROP COLUMN telegram_message_id;

CREATE TABLE task_signal_messages (
    task_id TEXT NOT NULL,
    recipient_kind TEXT NOT NULL CHECK (recipient_kind IN ('direct', 'group')),
    recipient_id TEXT NOT NULL,
    message_timestamp INTEGER NOT NULL,
    PRIMARY KEY (recipient_kind, recipient_id, message_timestamp)
);

CREATE INDEX idx_task_signal_messages_task_id ON task_signal_messages(task_id);
