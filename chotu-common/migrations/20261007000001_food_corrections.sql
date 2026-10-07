-- Anonymous Google nutrition logs are immutable: corrections delete then recreate.
-- Revision zero also tracks initial uploads, keeping their in-flight snapshot correctable.
CREATE TABLE food_log_corrections (
    food_log_id TEXT PRIMARY KEY NOT NULL REFERENCES food_log(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    sync_state TEXT NOT NULL CHECK (sync_state IN ('delete_pending', 'create_pending', 'synced')),
    remote_name TEXT,
    remote_snapshot_json TEXT,
    -- An earlier POST may still be running; resolve that named snapshot before deletion.
    remote_create_pending INTEGER NOT NULL DEFAULT 0 CHECK (remote_create_pending IN (0, 1)),
    replacement_name TEXT NOT NULL,
    CHECK (sync_state != 'delete_pending' OR remote_name IS NOT NULL),
    CHECK (remote_create_pending = 0 OR (remote_name IS NOT NULL AND remote_snapshot_json IS NOT NULL))
);
