-- Historical clock precision is unknown; never present reminder defaults as explicit times.
ALTER TABLE tasks ADD COLUMN due_has_time INTEGER NOT NULL DEFAULT 0;
