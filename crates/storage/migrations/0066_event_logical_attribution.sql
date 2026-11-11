-- Optional immutable request attribution in the existing event journal.
-- Existing/offline/system events remain explicitly unattributed.
ALTER TABLE events ADD COLUMN logical_agent_id TEXT;
CREATE INDEX idx_events_logical_seq ON events(logical_agent_id COLLATE NOCASE, seq DESC);
