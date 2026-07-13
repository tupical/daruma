-- Repo scope bindings (OSS task 019f5b73-84a5): `scope_path → project_id`
-- defaults, moved server-side from the per-process
-- `~/.agents/daruma/workspaces.json` so the same bindings work for stdio
-- and hosted (per-tenant) MCP sessions.
--
-- Plain config table, NOT event-sourced: bindings are per-installation
-- client convenience, not domain history worth replicating via sync.

CREATE TABLE IF NOT EXISTS repo_scopes (
    scope_path TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
