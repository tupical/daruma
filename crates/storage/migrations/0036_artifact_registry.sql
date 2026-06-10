-- Artifact Registry (P4)
-- Artifacts are named, versioned resources produced/consumed by agents.
-- Ownership is decoupled from the transient work-lease holder.

CREATE TABLE IF NOT EXISTS artifacts (
    id          TEXT PRIMARY KEY,
    uri         TEXT NOT NULL UNIQUE,
    title       TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'pending',
    owner_agent_id TEXT,
    task_id     TEXT,
    project_id  TEXT,
    version     TEXT,
    last_write_token INTEGER,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_artifacts_uri
    ON artifacts(uri);

CREATE INDEX IF NOT EXISTS idx_artifacts_project
    ON artifacts(project_id);

CREATE INDEX IF NOT EXISTS idx_artifacts_task
    ON artifacts(task_id);

CREATE INDEX IF NOT EXISTS idx_artifacts_status
    ON artifacts(status);

-- Typed directional relations between artifacts.
CREATE TABLE IF NOT EXISTS artifact_relations (
    id      TEXT PRIMARY KEY,
    from_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    to_id   TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    kind    TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(from_id, to_id, kind)
);

CREATE INDEX IF NOT EXISTS idx_artifact_relations_from
    ON artifact_relations(from_id);

CREATE INDEX IF NOT EXISTS idx_artifact_relations_to
    ON artifact_relations(to_id);
