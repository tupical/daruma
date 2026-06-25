---
description: Show the active daruma plan as a checklist with a progress bar.
---

The user invoked `/daruma-claude:plan`. Read-only — do not modify
tasks or write to `.omc/plans/`.

## Steps

1. Resolve project (`daruma_workspace_info` → `default_project`).
2. `daruma_plan_list` with `status = ["active", "in_progress"]`.
   Pick the most recently updated. If none, say
   "no active plan — `daruma_plan_create` first" and stop.
3. `daruma_plan_get` with the chosen `plan_id`.
4. Compute `done_count / total_count`. Build a 20-cell bar:
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

   - `[x]` only for `done`; everything else `[ ]`.
   - Status emoji: ⬜ todo, 🟢 in_progress, ✅ done, 📥 inbox.
   - Show priority only when `!= p2`.
   - Show `blocked-by` only when dependencies exist.

6. Suggest next action:
   - any `in_progress` → `→ continue: <title>`.
   - else any ready `todo` → `→ next: run /daruma-claude:next`.
   - else all done → `→ plan complete — daruma_plan_set_status status=done`.
