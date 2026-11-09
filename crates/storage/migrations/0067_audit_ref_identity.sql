DROP INDEX idx_audit_findings_dedup;

CREATE UNIQUE INDEX idx_audit_findings_legacy_dedup ON audit_findings (
    COALESCE(project_id, ''), check_key, COALESCE(plan_id, ''),
    COALESCE(task_id, ''), COALESCE(document_id, ''), COALESCE(artifact_id, '')
) WHERE refs = '[]';

CREATE UNIQUE INDEX idx_audit_findings_refs_dedup ON audit_findings (check_key, refs)
    WHERE refs <> '[]';
