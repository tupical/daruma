-- Paired-device identity and token binding (§3.3 Phase 5).
CREATE TABLE IF NOT EXISTS devices (
    id           TEXT PRIMARY KEY,
    label        TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    last_seen_at TEXT NULL,
    revoked_at   TEXT NULL
);

ALTER TABLE tokens ADD COLUMN device_id TEXT NULL REFERENCES devices(id);

CREATE INDEX IF NOT EXISTS idx_tokens_device_id ON tokens (device_id);
CREATE INDEX IF NOT EXISTS idx_devices_revoked_at ON devices (revoked_at);
