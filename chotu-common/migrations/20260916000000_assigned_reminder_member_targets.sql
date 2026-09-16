-- Assigned email reminders used to snapshot Signal ACI (or household group)
-- into email_task_signal_deliveries at enqueue. Rewriting those rows here is
-- unsafe on a fresh database: `tasks` is still the legacy email-classification
-- schema (no `assigned_to`) until `ensure_modern_tasks_schema` runs after
-- migrations. The rewrite lives in `database.rs`
-- (`rewrite_assigned_email_reminder_targets_to_members`).
SELECT 1;
