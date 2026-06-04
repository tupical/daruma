-- Event log: immutable, append-only.
CREATE TABLE IF NOT EXISTS events (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id     TEXT    NOT NULL UNIQUE,
    occurred_at  TEXT    NOT NULL,
    kind         TEXT    NOT NULL,
    actor_json   TEXT    NOT NULL,
    payload_json TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_events_kind ON events (kind);

-- Task projection (rebuilt from events on replay).
CREATE TABLE IF NOT EXISTS tasks (
    id          TEXT    PRIMARY KEY,
    project_id  TEXT    NULL,
    title       TEXT    NOT NULL,
    description TEXT    NOT NULL DEFAULT '',
    status      TEXT    NOT NULL DEFAULT 'inbox',
    priority    TEXT    NOT NULL DEFAULT 'p2',
    due_at      TEXT    NULL,
    created_at  TEXT    NOT NULL,
    updated_at  TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tasks_status     ON tasks (status);
CREATE INDEX IF NOT EXISTS idx_tasks_project_id ON tasks (project_id);

-- Project projection.
CREATE TABLE IF NOT EXISTS projects (
    id          TEXT    PRIMARY KEY,
    title       TEXT    NOT NULL,
    description TEXT    NULL,
    created_at  TEXT    NOT NULL,
    updated_at  TEXT    NOT NULL
);
