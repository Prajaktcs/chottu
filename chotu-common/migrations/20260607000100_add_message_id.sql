-- Migration: Add message_id column to tables processed by email stream and add unique constraints

-- 1. financial_ledger
ALTER TABLE financial_ledger ADD COLUMN message_id TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_financial_ledger_message_id ON financial_ledger(message_id);

-- 2. tasks
ALTER TABLE tasks ADD COLUMN message_id TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_tasks_message_id ON tasks(message_id);

-- 3. travel_itineraries
ALTER TABLE travel_itineraries ADD COLUMN message_id TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_travel_itineraries_message_id ON travel_itineraries(message_id);

-- 4. upcoming_bills
ALTER TABLE upcoming_bills ADD COLUMN message_id TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_upcoming_bills_message_id ON upcoming_bills(message_id);

-- 5. personal_references
ALTER TABLE personal_references ADD COLUMN message_id TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_personal_references_message_id ON personal_references(message_id);
