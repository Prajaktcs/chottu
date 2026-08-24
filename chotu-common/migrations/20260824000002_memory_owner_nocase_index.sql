-- Match the linked-DM owner comparison (`owner_member_id COLLATE NOCASE = ?`)
-- so SQLite can use this index for case-insensitive member-id lookups.
DROP INDEX IF EXISTS idx_memory_chunks_owner;

CREATE INDEX idx_memory_chunks_owner
    ON memory_chunks(owner_member_id COLLATE NOCASE);
