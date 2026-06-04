-- Tenant projection for logical workspace boundaries.

CREATE TABLE IF NOT EXISTS tenants (
    id                  TEXT PRIMARY KEY,
    name                TEXT NOT NULL,
    status              TEXT NOT NULL DEFAULT 'active',
    max_tasks           INTEGER NULL,
    max_plans           INTEGER NULL,
    max_storage_mb      INTEGER NULL,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);

INSERT OR IGNORE INTO tenants (
    id, name, status, created_at, updated_at
) VALUES (
    'self-hosted',
    'Self-hosted',
    'active',
    strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
    strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
);

ALTER TABLE projects ADD COLUMN tenant_id TEXT REFERENCES tenants(id);

UPDATE projects
SET tenant_id = 'self-hosted'
WHERE tenant_id IS NULL OR tenant_id = '';

CREATE INDEX IF NOT EXISTS idx_projects_tenant_id ON projects (tenant_id);

ALTER TABLE tokens ADD COLUMN tenant_id TEXT REFERENCES tenants(id);

CREATE INDEX IF NOT EXISTS idx_tokens_tenant_id ON tokens (tenant_id);
