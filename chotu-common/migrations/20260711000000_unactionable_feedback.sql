-- Migration: Add columns to tasks table and create unactionable_emails_feedback table.
ALTER TABLE tasks ADD COLUMN telegram_message_id INTEGER;
ALTER TABLE tasks ADD COLUMN email_sender TEXT;
ALTER TABLE tasks ADD COLUMN email_subject TEXT;

CREATE TABLE IF NOT EXISTS unactionable_emails_feedback (
    id TEXT PRIMARY KEY,
    sender TEXT NOT NULL,
    subject TEXT NOT NULL,
    body_preview TEXT,
    task_description TEXT,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
