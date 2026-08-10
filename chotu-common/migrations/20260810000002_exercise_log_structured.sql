-- Structured exercise columns for coach progress (activity type / duration / kcal / times).
-- Existing free-text `description` stays for display and legacy rows.
ALTER TABLE exercise_log ADD COLUMN activity_type TEXT;
ALTER TABLE exercise_log ADD COLUMN duration_minutes INTEGER;
ALTER TABLE exercise_log ADD COLUMN active_calories REAL;
ALTER TABLE exercise_log ADD COLUMN start_at TEXT;
ALTER TABLE exercise_log ADD COLUMN end_at TEXT;
