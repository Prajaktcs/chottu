-- Durable deletion is part of the initial/correction upload state machine.
-- Intents survive local deletion as tombstones, making retries idempotent and
-- preventing a stale worker from enrolling the same food ID after restart.
CREATE TABLE food_log_deletion_intents (
    food_log_id TEXT PRIMARY KEY NOT NULL,
    family_member_id TEXT NOT NULL,
    civil_date TEXT NOT NULL,
    original_timestamp TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    google_name TEXT,
    correction_remote_name TEXT,
    correction_replacement_name TEXT,
    correction_sync_state TEXT,
    completed INTEGER NOT NULL DEFAULT 0 CHECK (completed IN (0, 1))
);
CREATE INDEX food_log_deletion_intents_member_pending
    ON food_log_deletion_intents(family_member_id, completed);

-- Keep every known or possibly-created resource until a confirmed remote delete.
-- RESTRICT prevents an accidental local DELETE from cascading away cleanup work.
CREATE TABLE food_log_remote_resources (
    food_log_id TEXT NOT NULL REFERENCES food_log(id) ON DELETE RESTRICT,
    remote_name TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    snapshot_json TEXT,
    create_pending INTEGER NOT NULL DEFAULT 0 CHECK (create_pending IN (0, 1)),
    PRIMARY KEY (food_log_id, remote_name),
    CHECK (create_pending = 0 OR snapshot_json IS NOT NULL)
);
INSERT INTO food_log_remote_resources
    (food_log_id, remote_name, revision, snapshot_json, create_pending)
SELECT food_log_id, remote_name, revision, remote_snapshot_json, remote_create_pending
FROM food_log_corrections WHERE remote_name IS NOT NULL AND remote_name != '';
INSERT OR IGNORE INTO food_log_remote_resources
    (food_log_id, remote_name, revision, create_pending)
SELECT f.id, f.google_data_point_id, COALESCE(c.revision, 0), 0
FROM food_log f LEFT JOIN food_log_corrections c ON c.food_log_id = f.id
WHERE f.google_data_point_id IS NOT NULL AND f.google_data_point_id != '';
