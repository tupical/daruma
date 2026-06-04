-- Per-agent inbox cursor (Wave 3 / W3.1).
--
-- Each agent stores the highest `seq` it has ack'd so the long-poll
-- endpoint `/v1/agents/{id}/inbox` knows where to resume from across
-- reconnects. Updates are monotonic via `MAX(...)` so re-acking older
-- events is a no-op.
CREATE TABLE IF NOT EXISTS agent_acks (
    agent_id        TEXT    PRIMARY KEY,
    last_acked_seq  INTEGER NOT NULL DEFAULT 0,
    updated_at      TEXT    NOT NULL
);
