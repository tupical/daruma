-- Plan source chain (ADR-0009): a source is a node addressed by its
-- normalised URI (`note:<uuid>` when it has no link), pointing at the source
-- above it. One DB = one workspace, so `ref` is workspace-unique.
-- `plans.source_ref` names the nearest node. Projection of `SourceUpserted`.
CREATE TABLE sources (
    ref          TEXT PRIMARY KEY,
    label        TEXT,
    occurred_at  TEXT,
    note         TEXT,
    upstream_ref TEXT,
    created_at   TEXT NOT NULL,
    created_by   TEXT
);
CREATE INDEX idx_sources_upstream_ref ON sources(upstream_ref);
