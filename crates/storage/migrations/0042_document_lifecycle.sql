-- Document lifecycle + task binding (OSS task 019eb65b; vision.md rule 9,
-- "не документы, а артефакты задачи").
--
-- * `status`  — lifecycle status: draft | active | outdated | archived. The
--   minimum viable slice of the target taxonomy; TEXT so extending the set is
--   additive. Kept coherent with `archived_at` by the projector (entering
--   `archived` stamps it, leaving clears it). Existing rows get `active` —
--   the pre-lifecycle implicit state — so behaviour is unchanged.
-- * `task_id` — the task this document is an artifact of. NULL = the old
--   project-level shape. Consumers: Cloud rules 6-9 ("документ без задачи").
-- * `trigger_kind` / `consumer` — optional free-form creation metadata
--   ("what triggered this doc" / "who is expected to read it").
--
-- Rows archived before this migration keep status = 'active' + archived_at
-- set; the projector never reads status to decide visibility (queries filter
-- on archived_at), so the skew is cosmetic and self-heals on the next
-- status/archive event for the document.

ALTER TABLE documents ADD COLUMN status       TEXT NOT NULL DEFAULT 'active';
ALTER TABLE documents ADD COLUMN task_id      TEXT;
ALTER TABLE documents ADD COLUMN trigger_kind TEXT;
ALTER TABLE documents ADD COLUMN consumer     TEXT;

-- "Documents attached to this task" lookup (rule checks, UI panels).
CREATE INDEX IF NOT EXISTS idx_documents_task
    ON documents(task_id) WHERE task_id IS NOT NULL;
