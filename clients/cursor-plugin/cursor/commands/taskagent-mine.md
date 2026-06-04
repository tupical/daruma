---
name: taskagent-mine
description: Show tasks currently claimed by this agent / session.
---

# /taskagent-mine

Show tasks that the current MCP session has claimed (`in_progress` with
this session as owner). Useful for "what was I doing here?" after a chat
restart.

## Steps

1. Resolve project (`taskagent_workspace_info` → `default_project`).
2. `taskagent_list` with `project_id = <resolved>`,
   `status = ["in_progress"]`, `owner = "self"` (the server resolves
   `self` to the active session/agent). If the server does not support
   `owner = "self"`, fall back to `taskagent_list` then filter client-side
   by `owner == <agent_id from workspace_info>`.
3. Render:

   ```
   ## My active tasks — <N>

   | # | Pri | Title | Plan | Claimed |
   |---|-----|-------|------|---------|
   | 1 | p1 | <title>   | <plan_short> | <relative time, e.g. "2h ago"> |
   …
   ```

4. If the list is empty:

   ```
   No tasks claimed by this session.
   → run /taskagent-next to pick up the next ready task.
   ```

5. Read-only. Do not transition any task here.
