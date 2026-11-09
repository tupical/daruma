ALTER TABLE audit_findings ADD COLUMN refs TEXT NOT NULL DEFAULT '[]';

DROP INDEX idx_audit_findings_dedup;
CREATE UNIQUE INDEX idx_audit_findings_dedup ON audit_findings (
    COALESCE(project_id, ''), check_key, COALESCE(plan_id, ''),
    COALESCE(task_id, ''), COALESCE(document_id, ''),
    COALESCE(artifact_id, ''), refs
);
