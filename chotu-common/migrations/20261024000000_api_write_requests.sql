-- App API writes keyed by the client's request id, so a retried POST/DELETE
-- replays the stored response instead of applying twice (see docs/app-api.md).
CREATE TABLE IF NOT EXISTS api_write_requests (
    member_id TEXT NOT NULL,
    request_id TEXT NOT NULL,              -- client-generated `Idempotency-Key`
    request TEXT NOT NULL,                 -- "<METHOD> <path>" of the first attempt
    status INTEGER,                        -- NULL while the first attempt is in flight
    content_type TEXT,
    body BLOB,
    created_at TEXT NOT NULL,              -- RFC 3339 UTC; pruned after 7 days
    PRIMARY KEY (member_id, request_id)
);

CREATE INDEX IF NOT EXISTS idx_api_write_requests_created_at
    ON api_write_requests (created_at);
