-- §3.8.10 Provenance: track which brief produced which plan, and which
-- upstream event produced which task. Both columns are blob-style: the
-- application chooses how to populate them. Plans get a free-text
-- `source_brief`; tasks get an opaque `source_event_id` linking back to
-- the originating event in the same log.
--
-- Backfill: NULL for all pre-migration rows. The CTM B.4 spec calls
-- these `input-blob` fields — no PRD-first workflow is implied; the
-- columns are storage, not behaviour.

ALTER TABLE plans ADD COLUMN source_brief TEXT NULL;
ALTER TABLE tasks ADD COLUMN source_event_id TEXT NULL;
