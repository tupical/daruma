-- Denormalised status-change timestamp (OSS task 019eb675-5dc8; Audit primitives
-- task C heuristic 1). The `tasks` projection tracks `updated_at` (any mutation)
-- and `started_at` (first move to in_progress/done), but not "when did the task
-- enter its *current* status". The "stuck in current status longer than N" audit
-- needs exactly that, so we denormalise it from `TaskStatusChanged` /
-- `TaskCompleted` / `TaskReopened` rather than scanning the event log per query.
--
-- Backward compatible: existing rows backfill to `updated_at` (the best
-- available lower bound on the last status change) so the heuristic never treats
-- a long-lived task as freshly transitioned. New status changes overwrite it.

ALTER TABLE tasks ADD COLUMN status_changed_at TEXT;

UPDATE tasks SET status_changed_at = updated_at WHERE status_changed_at IS NULL;

-- "tasks in <status> whose status changed before <cutoff>" — filter by status,
-- then range-scan status_changed_at.
CREATE INDEX IF NOT EXISTS idx_tasks_status_changed
    ON tasks(status, status_changed_at);
