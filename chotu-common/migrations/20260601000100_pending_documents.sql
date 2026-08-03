-- Create pending_documents table to log PDFs that require LLM parsing

CREATE TABLE IF NOT EXISTS pending_documents (
    id TEXT PRIMARY KEY,
    filename TEXT NOT NULL,
    filepath TEXT NOT NULL,
    status TEXT NOT NULL,       -- 'PENDING', 'PROCESSED', 'FAILED'
    received_at DATETIME NOT NULL
);
