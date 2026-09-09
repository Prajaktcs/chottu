-- Cycle-scoped due-reminder delivery so a partial fan-out can retry only
-- failed recipients. Distinct from task_signal_messages, which keeps every
-- successful timestamp for reply correlation across snooze cycles.
CREATE TABLE task_due_reminder_deliveries (
    task_id TEXT NOT NULL,
    recipient_kind TEXT NOT NULL CHECK (recipient_kind IN ('direct', 'group')),
    recipient_id TEXT NOT NULL,
    PRIMARY KEY (task_id, recipient_kind, recipient_id)
);
