-- Historical project identifiers for slug/title lookup after renames.

CREATE TABLE IF NOT EXISTS project_identifier_aliases (
    alias      TEXT NOT NULL PRIMARY KEY,
    project_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_project_identifier_aliases_project
    ON project_identifier_aliases (project_id);
