---
name: taskagent-tasks
description: Show pending and in-progress tasks from taskagent as a compact table.
---

# /taskagent-tasks

Fetch the current task list from the taskagent MCP server and render it as
a markdown table.

## Steps

1. Resolve the active project:
   - Call `taskagent_workspace_info`. Use `default_project` if present.
   - Otherwise call `taskagent_project_list` and pick the first one. If
     none exist, tell the user "no projects yet — create one with
     `taskagent_project_create`" and stop.

2. Fetch tasks (filter on the server — do **not** load everything and
   filter locally):
   - `taskagent_list` with `project_id = <resolved>`, `status =
     ["inbox", "todo", "in_progress"]`. Limit to ~50.
   - **Never** use `status=all` in this command unless the user explicitly
     asked for the full archive — `all` is token-heavy.

3. Render exactly this format, nothing else:

   ```
   ## <project title> — <N> open tasks

   | # | Status | Pri | Title | Plan |
   |---|--------|-----|-------|------|
   | 1 | 🟢 in_progress | p1 | Wire installCommands into bin/install | plan-xxx |
   | 2 | ⬜ todo | p2 | Add tests for commands.mjs | plan-xxx |
   …
   ```

   Status emoji: 📥 inbox, ⬜ todo, 🟢 in_progress, ✅ done.
   Priority shown as-is (`p0`–`p3`). Title truncated to 60 chars with `…`.
   `Plan` column shows the short plan id (last 8 chars) or `—` if no plan.

4. If there are more than 30 rows, render the first 30 and add a footer:
   `…and <N> more — narrow with /taskagent-mine or a status filter`.

5. Do **not** invent IDs, statuses, or counts. If `taskagent_list` fails,
   surface the error verbatim and stop.

6. Do not write anything to `.omc/plans/` or markdown task files — this
   command is read-only.
