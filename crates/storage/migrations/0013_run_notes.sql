-- §3.8.2 — Free-form journal entries on a Run.
--
-- `RunNoteAppended` events project into this table. The author is denormalised
-- as the tagged JSON `Actor` (same shape as `comments.author_json`) so that
-- both user and agent authors are representable without an extra join.
--
-- Notes are append-only; there is no edit/delete command surface. `ON DELETE
-- CASCADE` mirrors how the run is the lifecycle owner — if a run row is ever
-- removed (e.g. test cleanup) its notes go with it.

CREATE TABLE IF NOT EXISTS run_notes (
    id          TEXT NOT NULL PRIMARY KEY,        -- RunNoteId (UUIDv7, "rnt_" prefix)
    run_id      TEXT NOT NULL,
    body        TEXT NOT NULL,
    author_json TEXT NOT NULL,                    -- tagged Actor JSON
    created_at  TEXT NOT NULL,                    -- RFC3339
    FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_run_notes_run ON run_notes (run_id, created_at);
