-- Activity projection (Section B.5 / Wave 1.1).
--
-- Denormalised, user-facing history of task and project changes.
-- Rows are immutable (append-only). `event_id` UNIQUE ensures idempotent
-- backfill: re-running `apply_event` for an existing event is a no-op.
-- `seq` mirrors the source event's global sequence number and is the
-- canonical cursor for pagination.
CREATE TABLE IF NOT EXISTS activity (
    id            TEXT    PRIMARY KEY,
    task_id       TEXT,                   -- nullable: project-level events
    project_id    TEXT,
    actor_json    TEXT    NOT NULL,       -- JSON-serialised Actor enum
    verb          TEXT    NOT NULL,       -- snake_case Verb name
    field         TEXT,                  -- e.g. "status", "priority"
    old_value     TEXT,
    new_value     TEXT,
    occurred_at   TEXT    NOT NULL,       -- ISO-8601 / RFC-3339
    event_id      TEXT    NOT NULL UNIQUE, -- idempotency key (backfill safe)
    seq           INTEGER NOT NULL        -- global event seq; cursor anchor
);

-- Primary lookup: all activity for a task, ordered for the UI feed.
CREATE INDEX IF NOT EXISTS idx_activity_task_seq
    ON activity (task_id, seq);

-- Secondary: project-level activity feed.
CREATE INDEX IF NOT EXISTS idx_activity_project_seq
    ON activity (project_id, seq);

-- Global ordering / cursor-pagination without task filter.
CREATE INDEX IF NOT EXISTS idx_activity_seq
    ON activity (seq);

-- Verb-filter support (`?verbs=closed,commented`).
CREATE INDEX IF NOT EXISTS idx_activity_verb
    ON activity (verb);
