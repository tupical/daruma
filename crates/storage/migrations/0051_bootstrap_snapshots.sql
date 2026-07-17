-- Bootstrap snapshots for device-sync catch-up (§3.3 Phase 5 follow-up).
--
-- A freshly paired device used to replay the whole event log from seq 0 via
-- EventStore::load_since. On a large workspace that is expensive, so the
-- server periodically materialises the write-through projection state
-- (tasks / projects / comments) into `snapshots`, labelled with the
-- event-log seq it was taken at. A new device restores the latest snapshot
-- and replays only the delta (load_since from snapshot.seq).
--
-- The event log is workspace-global, so snapshots are workspace-global too
-- (no per-project scope). payload_json holds a ProjectionSnapshot
-- ({ tasks, projects, comments }) — see crates/storage/src/snapshot_repo.rs.
CREATE TABLE IF NOT EXISTS snapshots (
    id           TEXT    PRIMARY KEY,
    seq          INTEGER NOT NULL,
    created_at   TEXT    NOT NULL,
    payload_json TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_snapshots_seq ON snapshots (seq);
