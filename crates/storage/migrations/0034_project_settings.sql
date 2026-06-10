-- Per-project settings (auto-append toggles for Interview / Human Log).
-- Key/value rows instead of a projects column: the projects projection
-- upserts with INSERT OR REPLACE over an explicit column list, which
-- would silently reset any column it doesn't know about.
CREATE TABLE IF NOT EXISTS project_settings (
    project_id TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (project_id, key)
);
