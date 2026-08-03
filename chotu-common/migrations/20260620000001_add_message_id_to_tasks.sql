-- Migration: Add message_id column to tasks table for email deduplication.
-- This runs on both fresh databases and migrated databases.

ALTER TABLE tasks ADD COLUMN message_id TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_tasks_message_id ON tasks(message_id);
