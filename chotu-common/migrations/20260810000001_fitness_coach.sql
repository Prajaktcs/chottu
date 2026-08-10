-- Persist Google Health exercise sessions for coaching + weekly plan context.
-- v1 stores free-text descriptions only. TODO: structured columns (activity_type,
-- duration_minutes, active_calories, start_at, end_at) for accurate progress.
CREATE TABLE IF NOT EXISTS exercise_log (
    id TEXT PRIMARY KEY,
    date TEXT NOT NULL,                    -- YYYY-MM-DD local civil day
    family_member_id TEXT NOT NULL,
    description TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'google_health',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_exercise_log_member_date
    ON exercise_log (family_member_id, date);

-- Weekly training plans generated from fitness_goals + recent activity.
CREATE TABLE IF NOT EXISTS fitness_weekly_plans (
    family_member_id TEXT NOT NULL,
    week_start TEXT NOT NULL,              -- Monday YYYY-MM-DD
    plan_md TEXT NOT NULL,
    plan_json TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (family_member_id, week_start)
);
