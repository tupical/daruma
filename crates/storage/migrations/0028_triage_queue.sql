-- Lightweight triage queue controls.

ALTER TABLE projects ADD COLUMN triage_enabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN triage_state TEXT NULL;

CREATE INDEX IF NOT EXISTS idx_tasks_triage_state
    ON tasks (triage_state);
