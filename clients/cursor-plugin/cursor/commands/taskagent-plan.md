---
name: taskagent-plan
description: Show the active plan's checklist with progress bar from taskagent.
---

# /taskagent-plan

Render the active execution plan as a markdown checklist with a progress
bar. Use the taskagent MCP server.

## Steps

1. Resolve project (`taskagent_workspace_info` → `default_project`).
2. `taskagent_plan_list` with `project_id = <resolved>`,
   `status = ["active", "in_progress"]`. Pick the most recently updated.
   If none, say "no active plan — create one with `taskagent_plan_create`"
   and stop.
3. `taskagent_plan_get` with the chosen `plan_id`.
4. Compute progress: `done_count / total_count`. Build a 20-cell bar like
   `▓▓▓▓▓▓▓▓░░░░░░░░░░░░ 40%`.
5. Render exactly:

   ```
   ## <plan title>

   Project: <project title>
   Progress: ▓▓▓▓▓▓▓▓░░░░░░░░░░░░ 40%  (4 / 10)
   Plan id:  <plan_id>

   ### Tasks

   - [x] ✅ <title>            — done
   - [ ] 🟢 <title>            — in_progress
   - [ ] ⬜ <title>            — todo (p1)
   - [ ] ⬜ <title>            — todo (p2, blocked-by <other-task-id>)
   …
   ```

   - `[x]` only for `done`; everything else is `[ ]`.
   - Status emoji: ⬜ todo, 🟢 in_progress, ✅ done, 📥 inbox.
   - Priority shown only when `!= p2`.
   - `blocked-by` rendered only when the dependency list is non-empty.

6. Below the list, suggest the next action one of these ways:
   - If any `in_progress` task exists → `→ continue: <title>`.
   - Else if any `todo` task is ready → `→ next: run /taskagent-next`.
   - Else if all done → `→ plan complete — run taskagent_plan_set_status status=done`.

7. Read-only — never modify tasks here. Don't touch `.omc/plans/` or
   markdown plan files.
