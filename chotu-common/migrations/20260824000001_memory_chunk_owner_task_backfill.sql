-- Backfill task chunk owners from live task rows so linked-DM search does
-- not treat preexisting assigned tasks as unassigned (NULL owner).
-- Journals/digests/refs stay NULL (household-only) until reindex/frontmatter.
UPDATE memory_chunks
SET owner_member_id = (
    SELECT assigned_to FROM tasks WHERE tasks.id = memory_chunks.source_id
)
WHERE source_type = 'task';
