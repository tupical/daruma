-- §3.7.5 — Webhook subscription `enrich` list (LIN B.4).
--
-- Allows a subscription to request pre-assembled context that the
-- dispatcher fans out *before* the POST, so weak subscribers receive
-- related data (parent_plan / project / task) without an extra
-- round-trip back to the server.
--
-- Stored as a JSON array of opaque string keys. Unknown keys are
-- ignored by the dispatcher at delivery time, so adding a new key
-- never requires a migration.
--
-- Existing rows: default `[]` keeps every pre-§3.7.5 subscription
-- behaviourally identical (no context is added unless the caller
-- opts in via PATCH).

ALTER TABLE webhooks
    ADD COLUMN enrich_json TEXT NOT NULL DEFAULT '[]';
