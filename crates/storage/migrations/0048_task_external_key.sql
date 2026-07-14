-- Idempotent task creation from external sources (webhooks, imports).
--
-- `external_key` uniquely identifies a task within the workspace so a
-- re-delivered "create" from an integration upserts onto the existing task
-- instead of spawning a duplicate. Nullable — tasks without an external
-- origin keep NULL and are unaffected.
--
-- Uniqueness is workspace-scoped: a single SQLite database is one workspace,
-- so a partial unique index over the non-NULL keys is the correct scope
-- (mirrors 0029_unique_event_origins.sql's `WHERE ... IS NOT NULL` pattern).
ALTER TABLE tasks ADD COLUMN external_key TEXT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_tasks_unique_external_key
    ON tasks (external_key)
    WHERE external_key IS NOT NULL;
