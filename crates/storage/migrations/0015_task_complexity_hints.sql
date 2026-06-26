-- §3.8.3 — Per-task complexity hints produced by a batch LLM analysis.
--
-- Materialised by `daruma_ai_analyze_complexity { plan_id }`: one LLM
-- call per *plan*, output upserted into this table keyed by `task_id`.
-- Re-running analysis simply overwrites the previous row (latest wins);
-- `batch_id` groups every row produced by the same run.
--
-- This is a **projection**, not part of the Task domain (see ROADMAP §3.8
-- — "complexity as a `Task` field" was explicitly rejected). Nothing in
-- core/storage depends on it; deleting the row is harmless.

CREATE TABLE IF NOT EXISTS task_complexity_hints (
    task_id              TEXT NOT NULL PRIMARY KEY,        -- TaskId
    score                INTEGER NOT NULL,                 -- 1..=10
    recommended_subtasks INTEGER NOT NULL,                 -- model-recommended fan-out
    expansion_hint       TEXT NOT NULL,                    -- short steering phrase for decompose
    reasoning            TEXT NOT NULL,                    -- model's explanation
    generated_at         TEXT NOT NULL,                    -- RFC3339
    batch_id             TEXT NOT NULL,                    -- groups one analyze_complexity run
    FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_task_complexity_hints_batch
    ON task_complexity_hints (batch_id);
