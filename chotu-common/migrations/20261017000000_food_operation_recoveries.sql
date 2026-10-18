-- Keep the authoritative mapping after meal cleanup so an import retry cannot
-- re-enroll an already-deleted resource or associate the Operation with another meal.
CREATE TABLE food_log_operation_recoveries (
    operation_name TEXT PRIMARY KEY NOT NULL,
    food_log_id TEXT NOT NULL,
    family_member_id TEXT NOT NULL,
    data_point_name TEXT NOT NULL UNIQUE,
    completed_response_json TEXT NOT NULL,
    recovered_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
