-- Outbound webhook subscriptions (Wave 3 / W3.3).
--
-- Each row describes one URL to fire on matching events. `events_json`
-- holds a JSON array of event-kind strings (e.g. ["task_reopened",
-- "task_commented"]); empty/null means "all kinds". `project_filter_json`
-- carries the serialised `ProjectFilter` (All vs Only).
CREATE TABLE IF NOT EXISTS webhooks (
    id                   TEXT    PRIMARY KEY,
    url                  TEXT    NOT NULL,
    secret               TEXT    NOT NULL,
    events_json          TEXT    NOT NULL DEFAULT '[]',
    project_filter_json  TEXT    NOT NULL DEFAULT '{"kind":"all"}',
    is_active            INTEGER NOT NULL DEFAULT 1,
    description          TEXT    NULL,
    created_at           TEXT    NOT NULL,
    updated_at           TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_webhooks_is_active ON webhooks (is_active);

-- Delivery log (kept in the same SQLite for MVP simplicity; the plan
-- recommends a separate file for production-grade retention/perf).
CREATE TABLE IF NOT EXISTS webhook_deliveries (
    id              TEXT    PRIMARY KEY,
    webhook_id      TEXT    NOT NULL,
    event_id        TEXT    NOT NULL,
    event_kind      TEXT    NOT NULL,
    status_code     INTEGER NULL,
    succeeded       INTEGER NOT NULL DEFAULT 0,
    attempts        INTEGER NOT NULL DEFAULT 1,
    error           TEXT    NULL,
    created_at      TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_webhook
    ON webhook_deliveries (webhook_id, created_at);
