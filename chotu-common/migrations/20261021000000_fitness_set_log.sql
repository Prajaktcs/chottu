-- Gym workouts logged set by set from Signal (/log), used for progressive overload.

-- One live workout per member; closed explicitly (`done`), by idle timeout, or day change.
CREATE TABLE IF NOT EXISTS fitness_workout_sessions (
    id TEXT PRIMARY KEY,
    family_member_id TEXT NOT NULL,
    date TEXT NOT NULL,                    -- YYYY-MM-DD local civil day
    current_exercise TEXT,                 -- exercise name sets are logged to; NULL until chosen
    started_at TEXT NOT NULL,              -- RFC 3339 UTC
    last_activity_at TEXT NOT NULL,        -- RFC 3339 UTC
    closed_at TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_fitness_workout_sessions_open
    ON fitness_workout_sessions (family_member_id)
    WHERE closed_at IS NULL;

CREATE TABLE IF NOT EXISTS fitness_set_log (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES fitness_workout_sessions (id),
    family_member_id TEXT NOT NULL,
    date TEXT NOT NULL,                    -- YYYY-MM-DD local civil day
    exercise_key TEXT NOT NULL,            -- normalized name; links sessions across weeks
    exercise_name TEXT NOT NULL,
    weight_lb REAL,                        -- NULL = bodyweight
    reps INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_fitness_set_log_member_exercise
    ON fitness_set_log (family_member_id, exercise_key, date);

CREATE INDEX IF NOT EXISTS idx_fitness_set_log_session
    ON fitness_set_log (session_id);
