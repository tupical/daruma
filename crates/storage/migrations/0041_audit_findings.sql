-- Audit findings store (OSS task 019eb674-f289; Audit primitives task B).
-- A finding is a problem a server-side audit check (script or, later, an AI
-- pass) raised about an entity: an unread document, a stuck task, a missing
-- owner, a duplicate-candidate pair, etc. Findings feed the Cloud Workspace
-- Audit surface; the OSS core only stores and serves them.
--
-- Unlike the evidence registry (0038), findings are NOT immutable: a check is
-- idempotent over `(project_id, check_key, entity_*)` — re-running it must update
-- the existing row's `last_seen_at`/`severity`/`detail` rather than insert a
-- duplicate, and a finding that no longer reproduces is auto-resolved. The
-- uniqueness is enforced by `idx_audit_findings_dedup` below; the repo upserts on
-- that key.
--
-- Zero-cost when unused: an empty table and unfired checks add nothing to the
-- hot path.

CREATE TABLE IF NOT EXISTS audit_findings (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL,
    -- Optional entity the finding is about (any subset; NULL = project-level).
    plan_id       TEXT,
    task_id       TEXT,
    document_id   TEXT,
    artifact_id   TEXT,
    -- Stable check identity for idempotency, e.g. 'doc.unread' or
    -- 'task.stuck_in_status'. Same check_key + same entity = same finding.
    check_key     TEXT NOT NULL,
    -- Free taxonomy bucket, e.g. 'hygiene', 'staleness', 'duplication'.
    category      TEXT NOT NULL,
    -- 'error' | 'warn' | 'info'.
    severity      TEXT NOT NULL,
    title         TEXT NOT NULL,
    detail        TEXT NOT NULL DEFAULT '',
    -- How to fix it (free text; surfaced to the operator).
    remediation   TEXT NOT NULL DEFAULT '',
    -- 'script' | 'ai' — who produced the finding.
    source        TEXT NOT NULL DEFAULT 'script',
    -- 'open' | 'acknowledged' | 'muted' | 'resolved'.
    status        TEXT NOT NULL DEFAULT 'open',
    first_seen_at TEXT NOT NULL,
    last_seen_at  TEXT NOT NULL,
    -- Set when a check stops reproducing the finding (auto-resolve) or an
    -- operator resolves it explicitly.
    resolved_by   TEXT,
    resolved_at   TEXT
);

-- Idempotency key: one live finding per (project, check, entity tuple). The
-- entity columns are coalesced to '' so NULLs participate in the unique key
-- (SQLite treats NULLs as distinct in UNIQUE indexes otherwise).
CREATE UNIQUE INDEX IF NOT EXISTS idx_audit_findings_dedup
    ON audit_findings(
        project_id,
        check_key,
        COALESCE(plan_id, ''),
        COALESCE(task_id, ''),
        COALESCE(document_id, ''),
        COALESCE(artifact_id, '')
    );

-- Listing with the common filters (status / severity / category) inside a
-- project, newest activity first.
CREATE INDEX IF NOT EXISTS idx_audit_findings_project
    ON audit_findings(project_id, status, severity, last_seen_at DESC);
