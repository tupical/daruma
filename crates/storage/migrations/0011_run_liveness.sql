-- §3.7.4 Liveness contract on Run (heartbeat).
--
-- last_activity_at — RFC3339 wall-clock of the most recent run "heartbeat":
--   * set to started_at on RunStarted
--   * refreshed on every RunStepStarted / RunStepFinished
-- unresponsive_at  — RFC3339 set once when RunUnresponsive has been emitted
--                    (no first step seen within liveness_ack_secs after RunStarted)
-- stale_at         — RFC3339 set once when RunStale has been emitted
--                    (no step activity for liveness_idle_secs)
--
-- All three are nullable; non-null on the *_at watchdog columns means the
-- corresponding signal-only event was already emitted for this run.

ALTER TABLE runs ADD COLUMN last_activity_at TEXT;
ALTER TABLE runs ADD COLUMN unresponsive_at  TEXT;
ALTER TABLE runs ADD COLUMN stale_at         TEXT;

CREATE INDEX IF NOT EXISTS runs_last_activity_idx
    ON runs(last_activity_at) WHERE status = 'active';
