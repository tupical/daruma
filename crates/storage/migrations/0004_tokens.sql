-- API token store for capability-based bearer auth (Wave 2 / W2.2).
--
-- The full token plaintext is **never** stored: `hash` carries the
-- argon2id PHC encoding. `prefix` is the first 12 characters of the
-- rendered token and is indexed so the middleware can find candidate
-- rows in O(1) before the (expensive) argon2 verify.
--
-- `scope_json` holds the serialised `TokenScope { projects, capabilities }`
-- — capabilities are a bitfield-encoded u32 wrapped in `Capabilities`.
CREATE TABLE IF NOT EXISTS tokens (
    id                  TEXT    PRIMARY KEY,
    prefix              TEXT    NOT NULL,
    hash                TEXT    NOT NULL,
    kind                TEXT    NOT NULL,
    agent_id            TEXT    NOT NULL,
    scope_json          TEXT    NOT NULL,
    rate_limit_per_min  INTEGER NOT NULL DEFAULT 60,
    created_at          TEXT    NOT NULL,
    expired_at          TEXT    NULL,
    last_used_at        TEXT    NULL,
    revoked_at          TEXT    NULL
);

-- Lookup-by-prefix is the hot path on every authed request.
CREATE INDEX IF NOT EXISTS idx_tokens_prefix   ON tokens (prefix);
CREATE INDEX IF NOT EXISTS idx_tokens_agent_id ON tokens (agent_id);
