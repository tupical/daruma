-- Artifact references attached to agent sessions.
--
-- Stores lightweight references only: file paths, URLs, or diff identifiers.
-- Large artifact bodies remain outside SQLite and are referenced by `ref`.

CREATE TABLE IF NOT EXISTS session_artifacts (
    id            TEXT NOT NULL PRIMARY KEY,
    session_id    TEXT NOT NULL,
    kind          TEXT NOT NULL CHECK (kind IN ('file', 'url', 'diff')),
    ref           TEXT NOT NULL,
    metadata_json TEXT NOT NULL DEFAULT 'null',
    created_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_session_artifacts_session_created
    ON session_artifacts (session_id, created_at ASC, id ASC);
