-- Run-step projection (§3.7).
--
-- `RunRepo::apply_event` previously only refreshed the run heartbeat
-- (`touch_activity`) on `RunStepStarted`/`RunStepFinished`, discarding the
-- per-step detail (which task, when started/finished, with what outcome). That
-- data survived only in the event log. This table materialises one row per
-- step so a run timeline can be served without replaying events.
--
-- `outcome` holds the full serialised `RunOutcome` JSON (not the lossy bare
-- string on `runs.outcome`) so `Failed { reason }` keeps its reason for the
-- timeline. Read back and re-parsed into a JSON object at query time.
CREATE TABLE IF NOT EXISTS run_steps (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id      TEXT NOT NULL,
    task_id     TEXT NOT NULL,
    started_at  TEXT NOT NULL,
    finished_at TEXT NULL,
    outcome     TEXT NULL
);

CREATE INDEX IF NOT EXISTS idx_run_steps_run
    ON run_steps(run_id);

-- Fast lookup of the open (unfinished) step for a run+task when a
-- `RunStepFinished` arrives.
CREATE INDEX IF NOT EXISTS idx_run_steps_open
    ON run_steps(run_id, task_id, finished_at);
