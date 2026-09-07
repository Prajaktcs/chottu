-- Replace Telegram reminder correlation with Signal recipient/timestamp mappings.
--
-- Do not ALTER TABLE ... DROP COLUMN here: that needs SQLite >= 3.35 and fails hard
-- on older libsqlite builds. `telegram_message_id` is removed in
-- `chotu-common::database::drop_tasks_telegram_message_id_if_present` (DROP COLUMN
-- with a rename+create+copy fallback) after migrations / modern-schema rebuild.

CREATE TABLE task_signal_messages (
    task_id TEXT NOT NULL,
    recipient_kind TEXT NOT NULL CHECK (recipient_kind IN ('direct', 'group')),
    recipient_id TEXT NOT NULL,
    message_timestamp INTEGER NOT NULL,
    PRIMARY KEY (recipient_kind, recipient_id, message_timestamp)
);

CREATE INDEX idx_task_signal_messages_task_id ON task_signal_messages(task_id);
