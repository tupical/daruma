-- Fast per-task diagnostics for the last projection-changing event.
--
-- The canonical audit trail remains the append-only `events` table plus the
-- task-scoped `activity` projection. These nullable columns make the current
-- task row self-describing for bug reports and list/detail responses: who last
-- changed it, and which event/seq to inspect for the full payload.

ALTER TABLE tasks ADD COLUMN updated_by_json TEXT NULL;
ALTER TABLE tasks ADD COLUMN updated_event_id TEXT NULL;
ALTER TABLE tasks ADD COLUMN updated_event_seq INTEGER NULL;
