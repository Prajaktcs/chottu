-- Task-owner backfill cannot run here: on a fresh database, `tasks` is still
-- the legacy email-classification schema (no `assigned_to`) until
-- `ensure_modern_tasks_schema` runs after migrations. The UPDATE lives in
-- `database.rs` (`backfill_memory_chunk_task_owners`).
SELECT 1;
