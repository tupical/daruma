---
name: daruma-next
description: Claim the next ready task from the active daruma plan and show its briefing.
---

# /daruma-next

Claim the next ready task from the active plan, set it to `in_progress`,
and render a compact briefing for the user.

## Steps

1. Resolve project (`daruma_workspace_info` → `default_project`).
2. Find the active plan: `daruma_plan_list` filtered to
   `status = ["active", "in_progress"]`, pick most recent.
   If none, stop with "no active plan — `daruma_plan_create` first".
3. Claim next: `daruma_plan_next_task` with the plan id. The server
   returns the next ready (unblocked) task and atomically transitions it
   to `in_progress` if it was `todo`.
4. If the server returns "no ready task" (plan empty or all blocked),
   render:

   ```
   No ready task. <reason from server, e.g. "3 tasks blocked by X">
   → run /daruma-plan to inspect dependencies.
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

   Then a `### Description` section with the task description verbatim
   (unwrapped), and if non-empty:

   ```
   ### Dependencies
   - <dep_task_id> — <dep_title> (status)
   ```

   ```
   ### Related (links)
   - <kind>: <related_task_id> — <title>
   ```

6. Finish with a short call-to-action:

   ```
   → When done: daruma_complete task_id=<task_id> [comment="<summary>"]
   → On failure: daruma_comment task_id=<task_id> body=<reason>
     followed by daruma_set_status task_id=<task_id> status=todo
   ```

7. Do not start executing the task yourself in this command — this is a
   briefing only. The user (or a follow-up agent prompt) drives execution.
