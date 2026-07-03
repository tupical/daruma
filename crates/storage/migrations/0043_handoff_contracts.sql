-- Handoff contracts (P5, OSS task 019ead4c-dc63): first-class gates between
-- work units. A non-accepted inbound handoff keeps the consuming unit out of
-- the `work_unit_drain_next` dispatch pool, so knowledge transfer stops
-- living only in comments.
--
-- One live contract per (from, to) pair: a re-request after rejection
-- reopens the same row (event carries the same id), so the projection never
-- accumulates stale blockers.
--
-- `status`: open | ready | accepted | rejected | expired. `ready` (artifacts
-- reached required_state) and `expired` (TTL sweep) are reserved for the
-- artifact-registry integration; today only open/accepted/rejected are wired.

CREATE TABLE IF NOT EXISTS handoff_contracts (
    id                    TEXT PRIMARY KEY,
    from_work_unit_id     TEXT NOT NULL,
    to_work_unit_id       TEXT NOT NULL,
    required_artifact_ids TEXT NOT NULL DEFAULT '[]', -- JSON array of URIs
    required_state        TEXT,                       -- draft|reviewed|approved|implemented|verified
    checklist             TEXT NOT NULL DEFAULT '[]', -- JSON array
    owner_agent_id        TEXT,
    accepted_by_agent_id  TEXT,
    status                TEXT NOT NULL DEFAULT 'open',
    notes                 TEXT,
    required_changes      TEXT NOT NULL DEFAULT '[]', -- JSON array
    created_at            TEXT NOT NULL,
    updated_at            TEXT NOT NULL,
    UNIQUE (from_work_unit_id, to_work_unit_id)
);

-- The drain-gate predicate: "does this unit have a non-accepted inbound
-- handoff?" filters by to_work_unit_id + status.
CREATE INDEX IF NOT EXISTS idx_handoffs_to_status
    ON handoff_contracts(to_work_unit_id, status);
