-- Manual tasks / timed reminders: due datetime + one-shot reminder marker.
ALTER TABLE tasks ADD COLUMN due_at TEXT;
ALTER TABLE tasks ADD COLUMN reminded_at TEXT;

CREATE INDEX IF NOT EXISTS idx_tasks_due_at ON tasks(due_at);
