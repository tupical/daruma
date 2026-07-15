-- ADR-0007 "plan-only intake" — legacy migration (decision Q2 + разбивка item 1).
--
-- Establishes the invariant "every task belongs to ≥1 plan" for all EXISTING
-- data by wrapping every plan-less task in a synthetic intake plan:
--
--   * one intake plan per project that has plan-less tasks, and
--   * one global intake plan (under a synthetic global project) for
--     project-less tasks (tasks.project_id IS NULL) — `plans.project_id` is
--     NOT NULL, so the global bucket needs a host project.
--
-- Task statuses are NOT touched. Each wrapped task gets its opaque provenance
-- slot `source_event_id` (migration 0017) stamped with a sentinel "migration
-- event" id, per the ADR ("source_event_id проставляется на событие миграции").
--
-- Non-destructive / logically reversible: this migration only ADDS `plans`,
-- `plan_tasks`, one synthetic `projects` row, and a provenance stamp. It never
-- deletes or rewrites task data, so a down-migration is a pure delete of the
-- rows it created (identified by the `daruma:legacy-intake` marker below) plus
-- clearing the sentinel `source_event_id`.
--
-- Idempotent: every insert is guarded by NOT EXISTS / a plan-less predicate,
-- so re-running against an already-wrapped workspace is a no-op. On a fresh /
-- empty DB there are no tasks, so every statement no-ops.
--
-- Synthetic intake plans are marked by `plans.source_brief = 'daruma:legacy-intake'`
-- (an opaque provenance blob per migration 0017). The runtime intake helpers in
-- `plan_repo.rs` (`PlanRepo::ensure_intake_plan` / `attach_task_to_intake`)
-- recognise the SAME marker and sentinel ids, so migration and runtime agree on
-- which plan is a project's intake wrapper.

-- ── 1. Per-project intake plans (one per project with plan-less tasks) ────────
-- randomblob() is evaluated once per DISTINCT project row, so each project gets
-- its own valid canonical UUID (8-4-4-4-12 hex; Uuid::parse_str accepts it).
INSERT INTO plans (
    id, project_id, parent_plan_id, title, description, goal,
    success_criteria_json, status, owner_json, created_at, updated_at,
    archived_at, source_brief
)
SELECT
    'pln_' || lower(
        hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-' ||
        hex(randomblob(2)) || '-' || hex(randomblob(2)) || '-' ||
        hex(randomblob(6))
    ),
    proj.project_id,
    NULL,
    'Legacy intake',
    '',
    'legacy intake migration',
    '[]',
    'draft',
    '{"kind":"user"}',
    strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
    strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
    NULL,
    'daruma:legacy-intake'
FROM (
    SELECT DISTINCT t.project_id AS project_id
    FROM tasks t
    WHERE t.project_id IS NOT NULL
      AND NOT EXISTS (SELECT 1 FROM plan_tasks pt WHERE pt.task_id = t.id)
) proj;

-- ── 2. Synthetic global project + plan (only if project-less tasks exist) ─────
-- Fixed sentinel ids (kept in sync with PlanRepo::GLOBAL_INTAKE_* constants).
INSERT INTO projects (
    id, title, description, created_at, updated_at, slug, tenant_id, triage_enabled
)
SELECT
    'prj_00000000-0000-7000-8000-0000000da0a1',
    '(legacy global intake)',
    'Synthetic project hosting the global intake plan for project-less legacy tasks (ADR-0007).',
    strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
    strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
    'p-legacy-global-intake',
    'self-hosted',
    0
WHERE EXISTS (
    SELECT 1 FROM tasks t
    WHERE t.project_id IS NULL
      AND NOT EXISTS (SELECT 1 FROM plan_tasks pt WHERE pt.task_id = t.id)
)
AND NOT EXISTS (
    SELECT 1 FROM projects WHERE id = 'prj_00000000-0000-7000-8000-0000000da0a1'
);

INSERT INTO plans (
    id, project_id, parent_plan_id, title, description, goal,
    success_criteria_json, status, owner_json, created_at, updated_at,
    archived_at, source_brief
)
SELECT
    'pln_00000000-0000-7000-8000-0000000da0a2',
    'prj_00000000-0000-7000-8000-0000000da0a1',
    NULL,
    'Legacy global intake',
    '',
    'legacy intake migration',
    '[]',
    'draft',
    '{"kind":"user"}',
    strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
    strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
    NULL,
    'daruma:legacy-intake'
WHERE EXISTS (
    SELECT 1 FROM tasks t
    WHERE t.project_id IS NULL
      AND NOT EXISTS (SELECT 1 FROM plan_tasks pt WHERE pt.task_id = t.id)
)
AND NOT EXISTS (
    SELECT 1 FROM plans WHERE id = 'pln_00000000-0000-7000-8000-0000000da0a2'
);

-- ── 3. Attach project-scoped plan-less tasks to their project's intake plan ───
-- position: dense 0-based per project, ordered by creation.
INSERT INTO plan_tasks (plan_id, task_id, position, depends_on_json)
SELECT
    p.id,
    t.id,
    row_number() OVER (PARTITION BY t.project_id ORDER BY t.created_at, t.id) - 1,
    '[]'
FROM tasks t
JOIN plans p
    ON p.project_id = t.project_id
   AND p.source_brief = 'daruma:legacy-intake'
WHERE t.project_id IS NOT NULL
  AND NOT EXISTS (SELECT 1 FROM plan_tasks pt WHERE pt.task_id = t.id);

-- ── 4. Attach project-less plan-less tasks to the global intake plan ──────────
INSERT INTO plan_tasks (plan_id, task_id, position, depends_on_json)
SELECT
    'pln_00000000-0000-7000-8000-0000000da0a2',
    t.id,
    row_number() OVER (ORDER BY t.created_at, t.id) - 1,
    '[]'
FROM tasks t
WHERE t.project_id IS NULL
  AND NOT EXISTS (SELECT 1 FROM plan_tasks pt WHERE pt.task_id = t.id);

-- ── 5. Stamp provenance on every task wrapped by this migration ───────────────
-- Sentinel "migration event" id (kept in sync with PlanRepo::MIGRATION_EVENT_ID).
-- Only stamps tasks that had no provenance yet, so real PlanCreated provenance
-- is never overwritten.
UPDATE tasks
SET source_event_id = 'evt_00000000-0000-7000-8000-0000000da0a3'
WHERE source_event_id IS NULL
  AND id IN (
    SELECT pt.task_id
    FROM plan_tasks pt
    JOIN plans p ON p.id = pt.plan_id
    WHERE p.source_brief = 'daruma:legacy-intake'
  );
