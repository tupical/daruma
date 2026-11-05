-- Consistency mark for bootstrap snapshots (crates/storage/src/snapshot_repo.rs).
--
-- Rows written before this migration read the live projections under a seq
-- taken earlier, so an event committed but not yet applied could be claimed
-- by `seq` and still be missing from `payload_json`. Those rows keep the
-- default 0 and are never served again. Snapshots folded from the event log
-- carry the current non-zero version.
ALTER TABLE snapshots ADD COLUMN cut_version INTEGER NOT NULL DEFAULT 0;
