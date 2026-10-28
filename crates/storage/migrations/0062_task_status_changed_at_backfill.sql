-- Backfill `tasks.status_changed_at` for rows the projection never stamped
-- (task 01a17eb7-2039-7622-88ed-4de282be9d53). Until now only
-- `TaskStatusChanged` stamped the column, so a task that never left its
-- initial status (the todo backlog) and a task closed through `TaskCompleted`
-- alone stayed NULL and were invisible to the "stuck in status" audit.
-- The projection now stamps `TaskCreated` and `TaskCompleted` too.
--
-- A NULL row had no `TaskStatusChanged` since migration 0040, so it is still in
-- the status it was created with (entered at `created_at`) — except `done`,
-- reached through `TaskCompleted` (entered at `completed_at`).
-- Backward compatible: only NULLs are filled; stamped rows are untouched.

UPDATE tasks
SET status_changed_at = CASE
        WHEN status = 'done' THEN COALESCE(completed_at, updated_at)
        ELSE created_at
    END
WHERE status_changed_at IS NULL;
