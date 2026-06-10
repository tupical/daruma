---
description: Refresh the open-task summary from the taskagent server and print it.
---

The user invoked `/taskagent-claude:sync`.

Pull the latest open tasks from the taskagent MCP server and display a
fresh summary. This is the manual equivalent of what the SessionStart hook
does automatically.

## Steps

1. Resolve project:
   - `taskagent_workspace_info` → use `default_project` if set.
   - Else `taskagent_project_list` → pick the first active project.
   - If none, print "No projects found — run `taskagent_project_create` first" and stop.

2. Fetch open tasks:
   - `taskagent_list` with `project_id = <resolved>`,
     `status = ["inbox", "todo", "in_progress", "in_review"]`, limit 50.

3. Render the summary:

   ```
   ## <project title> — <N> open tasks  (synced <timestamp>)

   | # | Status | Pri | Title | ID |
   |---|--------|-----|-------|----|
   | 1 | 🟢 in_progress | p1 | <truncated 60 chars> | <last-8 of id> |
   …
   ```

   - Status emoji: 📥 inbox, ⬜ todo, 🟢 in_progress, 🔍 in_review, ✅ done.
   - Title truncated to 60 chars with `…`.
   - Timestamp: ISO date-time in local timezone.

4. If 0 open tasks:
   ```
   ✅ No open tasks in "<project title>".
   → /taskagent-claude:start "<goal>" to create one.
   ```

5. If >30 rows: show first 30, add footer:
   `…and <N> more — use /taskagent-claude:tasks or filter by status.`

6. End with:
   ```
   → /taskagent-claude:next  claim the next ready task
   → /taskagent-claude:status <id>  details for a task
   ```

7. Read-only — do not transition any task in this command.
