-- Backfill only plan-owned tasks whose project is unambiguous. Tasks attached
-- to multiple plans are left untouched, even when their plan projects match:
-- migration must not guess ownership from historical projection state.
UPDATE tasks
SET project_id = (
    SELECT p.project_id
    FROM plan_tasks pt
    JOIN plans p ON p.id = pt.plan_id
    WHERE pt.task_id = tasks.id
      AND p.project_id IS NOT NULL
      AND p.project_id <> ''
)
WHERE project_id IS NULL
  AND 1 = (
      SELECT COUNT(*)
      FROM plan_tasks pt
      JOIN plans p ON p.id = pt.plan_id
      WHERE pt.task_id = tasks.id
        AND p.project_id IS NOT NULL
        AND p.project_id <> ''
  );

-- Bootstrap snapshots contain task projections. Drop stale cache entries so a
-- fresh replica cannot restore pre-backfill NULLs and skip the older add event.
DELETE FROM snapshots;
