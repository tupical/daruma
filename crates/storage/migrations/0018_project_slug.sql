-- Project slug for human-readable deep links (Remote: /{workspace}/{project-slug}).
--
-- Backfill MUST produce a unique value per row before the UNIQUE index is built.
-- Project ids are UUIDv7 (time-ordered) stored with a `prj_` prefix, so any short
-- prefix of the id (e.g. the previous `substr(id, 1, 8)`) repeats across every
-- project created in the same time window — which tripped the UNIQUE index with
-- "UNIQUE constraint failed: projects.slug" and aborted migration 18 entirely.
--
-- Derive the slug from the full id instead (the table's primary key, guaranteed
-- unique). The `prj_` prefix is stripped for readability; `replace` is a no-op
-- for any legacy rows stored without the prefix.
ALTER TABLE projects ADD COLUMN slug TEXT;

UPDATE projects
SET slug = 'p-' || replace(id, 'prj_', '')
WHERE slug IS NULL OR slug = '';

CREATE UNIQUE INDEX IF NOT EXISTS idx_projects_slug ON projects (slug);
