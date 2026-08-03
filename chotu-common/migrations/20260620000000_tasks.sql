-- Tasks / To-Dos table
-- Each task can optionally be linked to a Google Calendar event for auto-scheduling.

CREATE TABLE IF NOT EXISTS tasks (
    id                TEXT PRIMARY KEY,           -- UUID
    created_at        DATETIME NOT NULL,
    updated_at        DATETIME NOT NULL,
    title             TEXT NOT NULL,
    description       TEXT,
    assigned_to       TEXT,                       -- family_member_id or NULL = family-wide
    due_date          TEXT,                       -- YYYY-MM-DD, optional
    duration_minutes  INTEGER NOT NULL DEFAULT 30,-- estimated task duration
    priority          TEXT NOT NULL DEFAULT 'medium', -- low / medium / high
    status            TEXT NOT NULL DEFAULT 'open',   -- open / done / snoozed
    calendar_event_id TEXT,                       -- Google Calendar event ID if scheduled
    source            TEXT NOT NULL DEFAULT 'manual'  -- manual / inferred
);
