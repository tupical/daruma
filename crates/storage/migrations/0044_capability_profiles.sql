-- Agent capability profiles (P6, OSS task 019ead4c-f8a9): a DERIVED,
-- rebuildable projection mined from WorkUnit* events. Feeds scheduling as a
-- PREFERENCE (drain ordering), never a hard binding — a unit with zero fit
-- is still claimable by anyone.
--
-- * score            — 0..1 EWMA of outcome signals (completed=1.0,
--                      released-unfinished=0.4, blocked=0.3).
-- * confidence       — evidence_count / (evidence_count + 5).
-- * source           — 'inferred' (event mining) | 'user_set' (explicit
--                      override; mining never overwrites it, and it is
--                      exempt from the staleness cutoff — user wins).
-- * decay            — ponytail: instead of exponential decay in SQL, a
--                      profile older than 2 × decay_half_life_days simply
--                      stops contributing to drain ordering (hard staleness
--                      cutoff; upgrade to real decay if ordering quality
--                      ever warrants it).

CREATE TABLE IF NOT EXISTS agent_capability_profiles (
    agent_id              TEXT NOT NULL,
    capability            TEXT NOT NULL,
    score                 REAL NOT NULL DEFAULT 0,
    confidence            REAL NOT NULL DEFAULT 0,
    evidence_count        INTEGER NOT NULL DEFAULT 0,
    last_observed_at      TEXT NOT NULL,
    decay_half_life_days  REAL NOT NULL DEFAULT 30,
    source                TEXT NOT NULL DEFAULT 'inferred',
    updated_at            TEXT NOT NULL,
    PRIMARY KEY (agent_id, capability)
);
