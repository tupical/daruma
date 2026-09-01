-- Derive retention centrally so legacy writers remain compatible and mismatched
-- kind/payload rows fail safe as domain events.
ALTER TABLE events ADD COLUMN event_class TEXT GENERATED ALWAYS AS (
    CASE
        WHEN kind = 'operational_metric_recorded' AND json_valid(payload_json)
        THEN CASE
            WHEN json_extract(payload_json, '$.type') = 'operational_metric_recorded'
            THEN 'telemetry'
            ELSE 'domain'
        END
        ELSE 'domain'
    END
) VIRTUAL;

CREATE INDEX idx_events_class_occurred_at
    ON events (event_class, occurred_at);
