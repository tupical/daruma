-- P1 (WorkUnit + Artifact Ownership): generalize work_leases from
-- exclusive path globs to mode-aware resource leases with fencing.
--
--   mode          exclusive | shared_read | review | intent
--   target_uri    canonical resource URI (file://, artifact://,
--                 contract://, env://); NULL on pre-0033 rows, which are
--                 treated as exclusive file://<path_glob> leases
--   fencing_token monotonic per-resource counter issued at acquisition;
--                 stale holders cannot commit writes with an old token
ALTER TABLE work_leases ADD COLUMN mode TEXT NOT NULL DEFAULT 'exclusive';
ALTER TABLE work_leases ADD COLUMN target_uri TEXT;
ALTER TABLE work_leases ADD COLUMN fencing_token INTEGER;

CREATE TABLE IF NOT EXISTS lease_fencing_seq (
    resource_key TEXT PRIMARY KEY,
    seq          INTEGER NOT NULL DEFAULT 0
);
