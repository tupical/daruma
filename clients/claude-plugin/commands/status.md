---
description: Show detailed status for one task or a progress overview of the active plan.
---

The user invoked `/daruma-claude:status [<task_id_or_short>]`.

## Behaviour

### With a task ID argument

Show full details for that specific task.

1. `daruma_get id=<arg> comments=true` when `<arg>` is a full id (a UUID).
   A short suffix has to be resolved to a full id first — `daruma_search`
   matches title and description text only, never ids:
   a. `daruma_list status="active" project_id=<id> limit=100` (project from
      `daruma_workspace_info`).
   b. While the response has `has_more: true`, call the same list again with
      `cursor=<next_cursor>` and collect every page. One page is not the whole
      project: deciding "no match" or "exactly one match" from the first page
      misses tasks past the first 100. If a page fails to load, decide nothing
      and print `Could not read every open task — pass the full task id.`
   c. Keep the rows whose `id` ends with `<arg>` (case-insensitive).
   d. Only after the last page: exactly one match →
      `daruma_get id=<full id> comments=true`. No match →
      print `No open task ends with <arg> — pass the full task id.` and stop.
      Several matches → list them as `<full id> — <title>` and ask which one;
      show nothing until the user answers.
2. Render:

   ```
   ## <title>

   id:       <task_id>
   status:   <emoji> <status>
   priority: p<N>
   plan:     <plan_id or —>
   created:  <date>
   updated:  <date>

   ### Description
   <description verbatim>

   ### Dependencies  (only if non-empty)
   - <dep_id> — <dep_title> (<dep_status>)

   ### Comments  (last 5)
   [<date>] <author>: <body>
   …
   ```

3. End with transition hints based on current status:
   - `todo` / `inbox` → `→ daruma_set_status to in_progress when you start`
   - `in_progress` → `→ daruma_complete to close  |  daruma_comment to add a note`
   - `done` / `cancelled` → `→ task is closed`

### Without an argument — plan overview

Show progress for the active plan in the current project.

1. `daruma_workspace_info` → resolve `default_project`.
2. `daruma_plan_list status="active"` → most recent plan.
3. `daruma_plan_progress plan_id=<id>` for the progress bar.
4. Render:

   ```
   ## Plan: <plan title>

   Progress: [████████░░] 8/10 tasks done

   | Status | Count |
   |--------|-------|
   | ✅ done | 8 |
   | 🟢 in_progress | 1 |
   | ⬜ todo | 1 |

   Next ready task: <title> (<id short>)
   → /daruma-claude:next to claim it
   ```

5. If no active plan: `No active plan — /daruma-claude:tasks for raw task list.`

6. Read-only — do not transition any task.
