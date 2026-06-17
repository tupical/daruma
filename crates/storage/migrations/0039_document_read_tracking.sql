-- Document read-tracking (OSS task 019eb674-775c; Audit primitives task A).
-- Passive, lightweight columns on the `documents` projection that record when a
-- document was last read and by whom, plus a monotonic read counter. Updated in
-- place by the `GET /v1/documents/{id}` handler — NOT event-sourced: a read is
-- not a domain fact worth a row in the immutable log, and per-read events would
-- bloat it. A per (document, actor) throttle (≤ once/hour) keeps the write rate
-- low under repeated reads.
--
-- Distinct from the evidence `document_read_ack` (migration 0038): that is an
-- explicit, immutable acknowledgement a rule can require; this is passive usage
-- telemetry that powers the "documents not read in N days" audit heuristic.
--
-- Backward compatible: pre-existing rows have NULL `last_read_at` / NULL
-- `last_read_by` and `read_count = 0`, i.e. "never read".

ALTER TABLE documents ADD COLUMN last_read_at  TEXT;
ALTER TABLE documents ADD COLUMN last_read_by  TEXT;
ALTER TABLE documents ADD COLUMN read_count    INTEGER NOT NULL DEFAULT 0;

-- Cheap "documents in this project not read since <cutoff>" scan: filter by
-- project, then range/NULL-scan last_read_at. NULLs sort first in SQLite, so a
-- `last_read_at IS NULL OR last_read_at < ?` predicate is index-friendly.
CREATE INDEX IF NOT EXISTS idx_documents_last_read
    ON documents(project_id, last_read_at);
