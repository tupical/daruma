-- Evidence registry (OSS task 019eb65a-3185; docs/LIFECYCLE_RULES_SPEC.md §1.3).
-- Immutable proof that a lifecycle requirement was satisfied: a read-ack, an
-- impact assessment, a completion note, an owner assignment, etc. Columnar
-- projection of EvidenceRecorded events; the event log remains the source of
-- truth. The rule-engine gate reads this table to decide whether a `required`
-- rule blocks or passes.
--
-- Distinct from the artifact registry (0036): artifacts are production outputs,
-- evidence is process proof. They never share rows.

CREATE TABLE IF NOT EXISTS evidence (
    id            TEXT PRIMARY KEY,
    -- Evidence kind, 1:1 with Requirement.type_str(), e.g. 'document_read_ack'.
    kind          TEXT NOT NULL,
    -- Scope: 'tenant' | 'project' | 'plan' | 'task'. scope_id is NULL for tenant.
    scope_kind    TEXT NOT NULL,
    scope_id      TEXT,
    -- Optional discriminator matching a requirement target / doc_ref; NULL =
    -- applies regardless of target.
    target        TEXT,
    -- For document_read_ack: the read document version (entity_versions, 0020).
    doc_version   TEXT,
    -- Recorder, mirrors the entity_versions actor triple.
    actor_kind    TEXT NOT NULL,
    actor_id      TEXT,
    actor_name    TEXT,
    reason        TEXT NOT NULL DEFAULT '',
    -- JSON payload (required_fields content, structured assessment, …).
    payload       TEXT NOT NULL DEFAULT 'null',
    -- Optional bindings (any subset).
    project_id    TEXT,
    plan_id       TEXT,
    task_id       TEXT,
    run_id        TEXT,
    artifact_id   TEXT,
    rule_id       TEXT,
    recorded_at   TEXT NOT NULL,
    -- When set, a newer record superseded this one; the gate ignores it.
    superseded_by TEXT
);

-- Hot path: the gate looks up live (non-superseded) evidence for a
-- (scope, kind) pair while walking the scope chain.
CREATE INDEX IF NOT EXISTS idx_evidence_scope_kind
    ON evidence(scope_kind, scope_id, kind, superseded_by);

-- Listing evidence by binding (task/plan timelines).
CREATE INDEX IF NOT EXISTS idx_evidence_task ON evidence(task_id, recorded_at DESC);
CREATE INDEX IF NOT EXISTS idx_evidence_plan ON evidence(plan_id, recorded_at DESC);
CREATE INDEX IF NOT EXISTS idx_evidence_project ON evidence(project_id, recorded_at DESC);
