-- Preserve existing task displays; new manual tasks record whether a clock was supplied.
ALTER TABLE tasks ADD COLUMN due_has_time INTEGER NOT NULL DEFAULT 1;
