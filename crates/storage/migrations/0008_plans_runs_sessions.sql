-- Plans, Runs, Agent Sessions, Claims, External Refs, Idempotency (§3.1 Wave 1.2).
--
-- Provides the persistence layer for the plan-driven execution model:
-- plans own ordered task lists; runs track agent execution against a plan;
-- agent_sessions capture sub-agent lifecycle; agent_claims provide optimistic
-- task locking with TTL; external_refs enable idempotent creation from
-- external systems (e.g. OMC); processed_command_ids is the cross-cutting
-- idempotency table (Linear A.1 / ROADMAP §4.5).

CREATE TABLE IF NOT EXISTS plans (
    id                    TEXT    NOT NULL PRIMARY KEY,
    project_id            TEXT    NOT NULL,
    parent_plan_id        TEXT,
    title                 TEXT    NOT NULL,
    description           TEXT    NOT NULL DEFAULT '',
    goal                  TEXT    NOT NULL DEFAULT '',
    success_criteria_json TEXT    NOT NULL DEFAULT '[]',
    status                TEXT    NOT NULL DEFAULT 'draft',
    owner_json            TEXT    NOT NULL,
    created_at            TEXT    NOT NULL,
    updated_at            TEXT    NOT NULL,
    archived_at           TEXT
);

CREATE INDEX IF NOT EXISTS idx_plans_project ON plans (project_id);
CREATE INDEX IF NOT EXISTS idx_plans_parent  ON plans (parent_plan_id);
CREATE INDEX IF NOT EXISTS idx_plans_status  ON plans (status);   -- for list_active

CREATE TABLE IF NOT EXISTS plan_tasks (
    plan_id         TEXT    NOT NULL,
    task_id         TEXT    NOT NULL,
    position        INTEGER NOT NULL,
    depends_on_json TEXT    NOT NULL DEFAULT '[]',
    PRIMARY KEY (plan_id, task_id)
);

CREATE INDEX IF NOT EXISTS idx_plan_tasks_plan ON plan_tasks (plan_id, position);
CREATE INDEX IF NOT EXISTS idx_plan_tasks_task ON plan_tasks (task_id);

CREATE TABLE IF NOT EXISTS runs (
    id            TEXT    NOT NULL PRIMARY KEY,
    plan_id       TEXT    NOT NULL,
    agent_id      TEXT    NOT NULL,
    parent_run_id TEXT,
    started_at    TEXT    NOT NULL,
    ended_at      TEXT,
    status        TEXT    NOT NULL,
    outcome       TEXT
);

CREATE INDEX IF NOT EXISTS idx_runs_plan   ON runs (plan_id);
CREATE INDEX IF NOT EXISTS idx_runs_agent  ON runs (agent_id);
CREATE INDEX IF NOT EXISTS idx_runs_status ON runs (status);

-- Linear B.1: plan_steps_json stores the agent's live plan steps.
CREATE TABLE IF NOT EXISTS agent_sessions (
    id              TEXT    NOT NULL PRIMARY KEY,
    agent_id        TEXT    NOT NULL,
    parent_agent_id TEXT,
    started_at      TEXT    NOT NULL,
    ended_at        TEXT,
    metadata_json   TEXT    NOT NULL DEFAULT '{}',
    plan_steps_json TEXT    NOT NULL DEFAULT '[]'   -- Linear B.1
);

CREATE TABLE IF NOT EXISTS agent_claims (
    agent_id    TEXT    NOT NULL,
    task_id     TEXT    NOT NULL,
    acquired_at TEXT    NOT NULL,
    expires_at  TEXT    NOT NULL,
    PRIMARY KEY (agent_id, task_id)
);

CREATE INDEX IF NOT EXISTS idx_claims_task    ON agent_claims (task_id);
CREATE INDEX IF NOT EXISTS idx_claims_expires ON agent_claims (expires_at);

CREATE TABLE IF NOT EXISTS external_refs (
    tenant      TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    external_id TEXT    NOT NULL,
    internal_id TEXT    NOT NULL,
    created_at  TEXT    NOT NULL,
    PRIMARY KEY (tenant, kind, external_id)
);

CREATE INDEX IF NOT EXISTS idx_external_refs_internal ON external_refs (internal_id);

-- Cross-cutting idempotency store (Linear A.1 / ROADMAP §4.5).
-- TTL: rows older than 7 days are swept by a background task in the server.
CREATE TABLE IF NOT EXISTS processed_command_ids (
    client_command_id TEXT    NOT NULL PRIMARY KEY,
    server_event_id   TEXT    NOT NULL,
    server_event_seq  INTEGER NOT NULL,
    created_at        TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_processed_command_ids_created ON processed_command_ids (created_at);
