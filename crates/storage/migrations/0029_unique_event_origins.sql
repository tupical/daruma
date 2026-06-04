-- Prevent duplicate desktop-origin events during reconnect flush retries.

CREATE UNIQUE INDEX IF NOT EXISTS idx_events_unique_origin_device_seq
    ON events (origin_device_id, origin_seq)
    WHERE origin_device_id IS NOT NULL;
