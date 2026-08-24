-- Owner boundary for memory RAG: linked DMs must not retrieve the whole
-- household index. NULL owner_member_id = household-shared (journals/digests/
-- refs, unassigned tasks). Search in a linked DM keeps that member's rows plus
-- unassigned tasks only.
ALTER TABLE memory_chunks ADD COLUMN owner_member_id TEXT;

CREATE INDEX IF NOT EXISTS idx_memory_chunks_owner
    ON memory_chunks(owner_member_id);
