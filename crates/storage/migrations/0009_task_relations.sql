-- Task relations: typed directed edges between tasks (§3.2 Wave 1.2).
--
-- Supports three relation kinds: 'blocks' | 'relates_to' | 'duplicates'.
-- The UNIQUE constraint on (from_task, to_task, kind) catches duplicate
-- inserts that slip past the client_command_id idempotency layer.

CREATE TABLE task_relations (
    id           TEXT NOT NULL PRIMARY KEY,
    from_task    TEXT NOT NULL,
    to_task      TEXT NOT NULL,
    kind         TEXT NOT NULL,        -- 'blocks' | 'relates_to' | 'duplicates'
    created_at   TEXT NOT NULL,
    actor_json   TEXT NOT NULL,
    UNIQUE (from_task, to_task, kind)
);

CREATE INDEX idx_relations_from ON task_relations(from_task, kind);
CREATE INDEX idx_relations_to   ON task_relations(to_task,   kind);
