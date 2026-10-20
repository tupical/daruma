---
name: daruma-next
description: Claim the next ready task from the active daruma plan and show its briefing.
---

# /daruma-next

Claim the next ready task from the active plan, set it to `in_progress`,
and render a compact briefing for the user.

## Steps

1. Resolve project (`daruma_workspace_info` → `default_project`).
2. Find the active plan: `daruma_plan_list` with `status = "active"`,
   `sort = "updated"`, `limit = 1` — the most recently updated plan.
   If none, stop with "no active plan — `daruma_plan_materialize` first".
3. Claim next: `daruma_plan_drain_next` with `plan_id`. The server
   atomically claims the next ready (unblocked) task for this session and
   returns null when nothing is ready. Keep the returned `run_id`: the server
   opened that run for this claim. A null drain already aborted its own run —
   nothing to close.
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
   run:       <run_id>
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
     then daruma_run_complete run_id=<run_id>
   → On failure: daruma_comment task_id=<task_id> body=<reason>
     followed by daruma_set_status task_id=<task_id> status=todo
     and daruma_run_abort run_id=<run_id> reason=<reason>
   ```

   An open run keeps the plan from auto-closing after its last task; the
   `plan_close_blocked_by_run` warning names it in `details.run_id`.

7. Do not start executing the task yourself in this command — this is a
   briefing only. The user (or a follow-up agent prompt) drives execution.
