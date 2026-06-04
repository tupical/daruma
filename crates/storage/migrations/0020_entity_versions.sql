-- Version history for mutable user-facing entities.
--
-- This table implements docs/VERSION_HISTORY.md. It intentionally uses one
-- shared table instead of task_versions/doc_versions so read APIs, rollback,
-- and retention policy can stay entity-agnostic.
--
-- Snapshot and diff payloads are JSON stored as TEXT, matching the existing
-- storage convention for Actor/Event payloads.

CREATE TABLE IF NOT EXISTS entity_versions (
    id                  TEXT    NOT NULL PRIMARY KEY, -- VersionId (UUIDv7, "ver_" prefix)
    entity_type         TEXT    NOT NULL CHECK (entity_type IN ('task', 'document')),
    entity_id           TEXT    NOT NULL,             -- TaskId or DocumentId
    version_number      INTEGER NOT NULL CHECK (version_number > 0),

    actor_json          TEXT    NOT NULL,             -- JSON-serialised Actor enum
    actor_kind          TEXT    NOT NULL,             -- 'user' | 'agent'
    actor_id            TEXT,                         -- AgentId for agents; NULL for current user actor
    actor_name          TEXT,                         -- agent display name when present

    event_type          TEXT    NOT NULL,             -- stable Event::kind()
    reason              TEXT,
    source_event_id     TEXT,
    source_event_seq    INTEGER,
    created_at          TEXT    NOT NULL,             -- RFC3339

    before_json         TEXT,                         -- NULL for first version
    after_json          TEXT,                         -- NULL only if hard-delete is ever introduced
    diff_json           TEXT    NOT NULL,             -- structured diff envelope
    changed_fields_json TEXT    NOT NULL DEFAULT '[]',
    summary             TEXT    NOT NULL,

    UNIQUE (entity_type, entity_id, version_number)
);

-- Primary history lookup for `GET /history?entity=...`.
CREATE INDEX IF NOT EXISTS idx_entity_versions_entity_version
    ON entity_versions (entity_type, entity_id, version_number DESC);

-- Cursor pagination for an entity timeline.
CREATE INDEX IF NOT EXISTS idx_entity_versions_entity_created
    ON entity_versions (entity_type, entity_id, created_at DESC, id DESC);

-- Global/latest-changes feed.
CREATE INDEX IF NOT EXISTS idx_entity_versions_created
    ON entity_versions (created_at DESC, id DESC);

-- Actor-scoped audit queries. `actor_id` is NULL for the current `Actor::User`
-- shape, so `actor_kind` remains part of the key.
CREATE INDEX IF NOT EXISTS idx_entity_versions_actor_id
    ON entity_versions (actor_kind, actor_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_entity_versions_actor_name
    ON entity_versions (actor_kind, actor_name, created_at DESC);

-- Idempotency/debug path from source event to version record.
CREATE UNIQUE INDEX IF NOT EXISTS idx_entity_versions_source_event
    ON entity_versions (entity_type, entity_id, source_event_id)
    WHERE source_event_id IS NOT NULL;
