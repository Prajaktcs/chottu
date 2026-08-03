-- Local RAG index: chunked journals / digests / personal refs / tasks with embeddings.
CREATE TABLE IF NOT EXISTS memory_chunks (
    id TEXT PRIMARY KEY NOT NULL,
    source_type TEXT NOT NULL,
    source_id TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    url TEXT,
    occurred_at TEXT,
    content_hash TEXT NOT NULL,
    embedding BLOB NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_chunks_source
    ON memory_chunks(source_type, source_id);

CREATE INDEX IF NOT EXISTS idx_memory_chunks_occurred
    ON memory_chunks(occurred_at);
