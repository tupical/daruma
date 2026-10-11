-- Role artifacts (ADR-0011 п.3): a role's structured result lives on an
-- existing `artifacts` row, not in a new table. All columns are NULL for
-- plain registry artifacts, which keep their previous behaviour.
-- `role_status` is a second axis (proposed|superseded|applied|rejected),
-- independent of the registry lifecycle `status`. Not backfilled.
ALTER TABLE artifacts ADD COLUMN role TEXT;
ALTER TABLE artifacts ADD COLUMN role_kind TEXT;
ALTER TABLE artifacts ADD COLUMN role_generation INTEGER;
ALTER TABLE artifacts ADD COLUMN role_status TEXT;
ALTER TABLE artifacts ADD COLUMN role_payload_json TEXT;

-- "All artifacts kind=X status=proposed for task N".
CREATE INDEX IF NOT EXISTS idx_artifacts_task_role_kind_status
    ON artifacts(task_id, role_kind, role_status)
    WHERE role IS NOT NULL;

-- One generation per (task, role, kind).
CREATE UNIQUE INDEX IF NOT EXISTS idx_artifacts_role_generation
    ON artifacts(task_id, role, role_kind, role_generation)
    WHERE role IS NOT NULL;
