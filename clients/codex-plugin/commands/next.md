---
description: Claim the next ready task from the active daruma plan and show its briefing.
---

The user invoked `/daruma-claude:next`. This claims a task —
afterward, control returns to the user/agent to actually execute it.

## Steps

1. Resolve project (`daruma_workspace_info` → `default_project`).
2. Active plan: `daruma_plan_list` filtered to
   `status = ["active", "in_progress"]`, most recent.
   If none, stop with "no active plan — `daruma_plan_create` first".
3. `daruma_plan_next_task` with the plan id. Server atomically picks
   the next ready (unblocked) task and transitions `todo → in_progress`.
4. If "no ready task":

   ```
   No ready task. <reason from server, e.g. "3 tasks blocked by X">
   → run /daruma-claude:plan to inspect dependencies.
   ```

   Stop.

5. Otherwise render the briefing:

   ```
   ## Next task: <title>

   id:        <task_id>
   plan:      <plan_id>
   priority:  <pX>
   status:    🟢 in_progress
   ```

   Then `### Description` with the task description verbatim, and when
   non-empty:

   ```
   ### Dependencies
   - <dep_task_id> — <dep_title> (status)

   ### Related (links)
   - <kind>: <related_task_id> — <title>
   ```

6. End with:

   ```
   → When done: daruma_complete task_id=<task_id> [comment="<summary>"]
   → On failure: daruma_comment task_id=<task_id> body=<reason>
     followed by daruma_set_status task_id=<task_id> status=todo
   ```

7. Do not start executing the task itself in this command — it's a
   briefing only. Wait for the user (or an explicit follow-up).
