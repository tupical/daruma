---
description: Show the active daruma plan as a checklist with a progress bar.
---

The user invoked `/daruma-claude:plan`. Read-only — do not modify
tasks or write to `.omc/plans/`.

## Steps

1. Resolve project (`daruma_workspace_info` → `default_project`).
2. `daruma_plan_list` with `status = "active"`, `sort = "updated"`,
   `limit = 1` — the server orders by `updated_at`, newest first, so the
   single row is the most recently updated plan (the default order is by
   creation, and its first page can miss it). If none, say
   "no active plan — `daruma_plan_materialize` first" and stop.
3. `daruma_plan_get` with the chosen `plan_id` and `view = "tasks"` —
   the only view that lists every plan task (done included) with
   `status`, `priority`, `depends_on` and `blocked_by`. The default
   `progress` view caps each group at five open tasks; don't use it here.
4. Compute `progress.tasks_done / progress.tasks_total`. Build a 20-cell bar:
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
   - Status emoji: ⬜ todo, 🟢 in_progress, 🔍 in_review, ✅ done,
     ❌ cancelled, 📥 inbox.
   - Show priority only when `!= p2`.
   - Show `blocked-by` only when `blocked_by` is non-empty (unfinished
     `depends_on` tasks and active `blocks` relations).

6. Suggest next action:
   - any `in_progress` → `→ continue: <title>`.
   - else any ready `todo` → `→ next: run /daruma-claude:next`.
   - else all done → `→ plan complete — daruma_plan_set_status status=completed`.
