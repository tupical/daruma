-- Multi-device event origin metadata.

ALTER TABLE events ADD COLUMN origin_device_id TEXT NULL;
ALTER TABLE events ADD COLUMN origin_seq INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_events_origin_device_seq
    ON events (origin_device_id, origin_seq);
