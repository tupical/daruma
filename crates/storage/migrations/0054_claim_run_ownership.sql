-- Preserve pre-0054 claims while adding an immutable generation. Legacy rows
-- cannot be attributed to a run safely: Run.agent_id is caller-provided and is
-- distinct from the authenticated claim holder, so their run_id remains NULL.
CREATE TABLE agent_claims_v0054 (
    agent_id    TEXT NOT NULL,
    task_id     TEXT NOT NULL,
    acquired_at TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    run_id      TEXT,
    claim_id    TEXT NOT NULL UNIQUE,
    PRIMARY KEY (agent_id, task_id)
);

INSERT INTO agent_claims_v0054
    (agent_id, task_id, acquired_at, expires_at, run_id, claim_id)
SELECT agent_id, task_id, acquired_at, expires_at, NULL,
       'clm_' || lower(
           hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-' ||
           hex(randomblob(2)) || '-' || hex(randomblob(2)) || '-' ||
           hex(randomblob(6))
       )
FROM agent_claims;

DROP TABLE agent_claims;
ALTER TABLE agent_claims_v0054 RENAME TO agent_claims;

CREATE INDEX idx_claims_task    ON agent_claims (task_id);
CREATE INDEX idx_claims_expires ON agent_claims (expires_at);
CREATE INDEX idx_claims_run     ON agent_claims (run_id);

CREATE TABLE run_claim_owners (
    run_id   TEXT NOT NULL PRIMARY KEY,
    agent_id TEXT NOT NULL
);
