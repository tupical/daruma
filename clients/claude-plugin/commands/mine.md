---
description: Show taskagent tasks currently claimed by this Claude Code session.
---

The user invoked `/taskagent-claude:mine`. Read-only.

## Steps

1. Resolve project (`taskagent_workspace_info` → `default_project`,
   capture the agent/session id from the response).
2. `taskagent_list` with `project_id = <resolved>`,
   `status = ["in_progress"]`, `owner = "self"`. If the server rejects
   `owner = "self"`, list everything in `in_progress` and filter
   client-side by `owner == <agent_id from workspace_info>`.
3. Render:

   ```
   ## My active tasks — <N>

   | # | Pri | Title | Plan | Claimed |
   |---|-----|-------|------|---------|
   | 1 | p1 | <title> | <plan_short> | <relative time, e.g. "2h ago"> |
   …
   ```

4. If empty:

   ```
   No tasks claimed by this session.
   → run /taskagent-claude:next to pick up the next ready task.
   ```

5. Do not transition any task here.
