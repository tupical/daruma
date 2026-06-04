-- Add lifecycle timestamps for semantic events (Wave 2 / W2.1).
--
-- * `started_at`   — set when a task first transitions out of Inbox/Todo into
--                    InProgress (or directly into Done from non-terminal).
-- * `completed_at` — set on TaskClosed (terminal transition), cleared on
--                    TaskReopened.
--
-- Both columns are nullable so the migration is non-breaking for existing rows.
ALTER TABLE tasks ADD COLUMN started_at   TEXT NULL;
ALTER TABLE tasks ADD COLUMN completed_at TEXT NULL;
