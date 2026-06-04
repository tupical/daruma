-- §3.5 Authorship: persist the creator and completer on each task row.
--
-- created_by_json  — JSON-encoded Actor set on TaskCreated
-- completed_by_json — JSON-encoded Actor set on TaskCompleted / TaskStatusChanged{to=done}
--
-- Both columns are NULL for tasks created before this migration; backfill
-- policy is intentionally deferred (see §3.7.11 / T9 policy doc).
-- sqlx tracks applied migrations by filename + checksum, so each statement
-- runs exactly once per database — no IF NOT EXISTS guard needed.

ALTER TABLE tasks ADD COLUMN created_by_json   TEXT NULL;
ALTER TABLE tasks ADD COLUMN completed_by_json TEXT NULL;
