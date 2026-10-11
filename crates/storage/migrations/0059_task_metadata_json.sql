-- Free-form task metadata as a JSON object (ADR-0011 п.2). First key:
-- `pipeline_position` {stage_index, role, complete} — where the task is in its
-- workspace workflow_policy. NULL for tasks that carry no metadata; such tasks
-- keep their previous behaviour. Not backfilled.
ALTER TABLE tasks ADD COLUMN metadata_json TEXT;
