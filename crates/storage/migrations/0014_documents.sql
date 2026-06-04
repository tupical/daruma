-- PR1 §3 — Documents: markdown artefacts attached to a Project.
--
-- A `Document` is a free-form markdown blob owned by a project. `kind`
-- discriminates the two default slots (`interview`, `human_log`) that the
-- handler auto-creates on `Command::CreateProject`; additional documents
-- of either kind may be created freely (kind is NOT unique per project).
--
-- `archived_at` is the soft-delete column: `NULL` = active, non-NULL =
-- archived but still queryable via `list_by_project(include_archived=true)`.
-- All five `Document*` events project into this table; see
-- `crates/storage/src/document_repo.rs`.

CREATE TABLE IF NOT EXISTS documents (
    id          TEXT NOT NULL PRIMARY KEY,        -- DocumentId (UUIDv7, "doc_" prefix)
    project_id  TEXT NOT NULL,
    kind        TEXT NOT NULL,                    -- DocumentKind::as_str() ("interview" | "human_log")
    title       TEXT NOT NULL,
    content     TEXT NOT NULL DEFAULT '',         -- raw markdown
    created_at  TEXT NOT NULL,                    -- RFC3339
    updated_at  TEXT NOT NULL,                    -- RFC3339
    archived_at TEXT                              -- RFC3339 when soft-archived, else NULL
);

CREATE INDEX IF NOT EXISTS idx_documents_project ON documents (project_id);
CREATE INDEX IF NOT EXISTS idx_documents_kind    ON documents (project_id, kind);
