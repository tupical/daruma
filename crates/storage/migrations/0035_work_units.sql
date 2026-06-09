-- P3 (WorkUnit + Artifact Ownership): the minimal dispatchable unit,
-- subordinate to a task. Mirrors the plans/runs projection template.
CREATE TABLE IF NOT EXISTS work_units (
    id                   TEXT NOT NULL PRIMARY KEY,
    task_id              TEXT NOT NULL,
    stage_plan_id        TEXT,
    title                TEXT NOT NULL,
    description          TEXT NOT NULL DEFAULT '',
    status               TEXT NOT NULL DEFAULT 'todo',
    priority             TEXT NOT NULL DEFAULT 'p2',
    capability_tags_json TEXT NOT NULL DEFAULT '[]',
    owner_agent_id       TEXT,
    claim_expires_at     TEXT,
    artifact_refs_json   TEXT NOT NULL DEFAULT '[]',
    acceptance_json      TEXT NOT NULL DEFAULT '[]',
    created_at           TEXT NOT NULL,
    updated_at           TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_work_units_task    ON work_units (task_id);
CREATE INDEX IF NOT EXISTS idx_work_units_status  ON work_units (status);
CREATE INDEX IF NOT EXISTS idx_work_units_owner   ON work_units (owner_agent_id);
