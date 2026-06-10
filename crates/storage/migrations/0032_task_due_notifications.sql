-- §task.due webhook: one notification per (task, due_at) value.
--
-- Separate projection table (NOT a tasks column) because the tasks
-- projection upserts with INSERT OR REPLACE over an explicit column
-- list — an extra column there would be silently reset on every task
-- update. A (task_id, due_at) row here means "task.due already emitted
-- for this deadline"; changing due_at re-arms the notification because
-- the stored due_at no longer matches.
CREATE TABLE IF NOT EXISTS task_due_notifications (
    task_id     TEXT PRIMARY KEY,
    due_at      TEXT NOT NULL,
    notified_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tasks_due_at ON tasks(due_at) WHERE due_at IS NOT NULL;
