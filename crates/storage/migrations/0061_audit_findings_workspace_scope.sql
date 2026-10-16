-- Workspace-scope audit findings (AUD-5, task 01a12f28-ac34-7401-bf28-ca96c5a16505).
-- Some signals describe the whole workspace, not a project: MCP usage
-- telemetry without a verified project routes to no project. Before this
-- migration such a signal was copied into every project. `project_id IS NULL`
-- now means "workspace finding"; no fake project is introduced.
--
-- SQLite cannot drop NOT NULL in place, so the table is rebuilt. Rows keep
-- their ids, timestamps, statuses and operator decisions (provenance stays).
-- The dedup key coalesces project_id like the entity columns so exactly one
-- live workspace finding exists per (check_key, entity tuple).

CREATE TABLE audit_findings_v0061 (
    id            TEXT PRIMARY KEY,
    -- NULL = workspace-scope finding (not attributable to one project).
    project_id    TEXT,
    plan_id       TEXT,
    task_id       TEXT,
    document_id   TEXT,
    artifact_id   TEXT,
    check_key     TEXT NOT NULL,
    category      TEXT NOT NULL,
    severity      TEXT NOT NULL,
    title         TEXT NOT NULL,
    detail        TEXT NOT NULL DEFAULT '',
    remediation   TEXT NOT NULL DEFAULT '',
    source        TEXT NOT NULL DEFAULT 'script',
    status        TEXT NOT NULL DEFAULT 'open',
    first_seen_at TEXT NOT NULL,
    last_seen_at  TEXT NOT NULL,
    resolved_by   TEXT,
    resolved_at   TEXT
);

INSERT INTO audit_findings_v0061
    (id, project_id, plan_id, task_id, document_id, artifact_id, check_key,
     category, severity, title, detail, remediation, source, status,
     first_seen_at, last_seen_at, resolved_by, resolved_at)
SELECT id, project_id, plan_id, task_id, document_id, artifact_id, check_key,
       category, severity, title, detail, remediation, source, status,
       first_seen_at, last_seen_at, resolved_by, resolved_at
FROM audit_findings;

DROP TABLE audit_findings;
ALTER TABLE audit_findings_v0061 RENAME TO audit_findings;

CREATE UNIQUE INDEX IF NOT EXISTS idx_audit_findings_dedup
    ON audit_findings(
        COALESCE(project_id, ''),
        check_key,
        COALESCE(plan_id, ''),
        COALESCE(task_id, ''),
        COALESCE(document_id, ''),
        COALESCE(artifact_id, '')
    );

CREATE INDEX IF NOT EXISTS idx_audit_findings_project
    ON audit_findings(project_id, status, severity, last_seen_at DESC);
