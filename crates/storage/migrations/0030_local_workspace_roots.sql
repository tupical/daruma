-- Local logical workspace/project root bindings.
--
-- These tables are OSS/self-host metadata for one-server local usage. They do
-- not create extra SQLite databases; they attach filesystem roots to existing
-- tenants (logical workspaces) and projects.

CREATE TABLE IF NOT EXISTS workspace_roots (
    id          TEXT PRIMARY KEY,
    tenant_id   TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    root_path   TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_workspace_roots_tenant_id
    ON workspace_roots (tenant_id);

CREATE TABLE IF NOT EXISTS project_roots (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    root_path   TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_project_roots_project_id
    ON project_roots (project_id);
