---
name: daruma-mine
description: Show tasks currently claimed by this agent / session.
---

# /daruma-mine

Show tasks that the current MCP session has claimed (`in_progress` with
an active claim held by this session's agent). Useful for "what was I doing here?" after a chat
restart.

## Steps

1. Resolve project (`daruma_workspace_info` → `default_project`) and
   capture `mcp_agent_id` from the same response — that is this session's
   agent id.
2. `daruma_list` with `project_id = <resolved>`, `status = "in_progress"`
   (a single string, not an array), limit 20. `daruma_list` has no owner
   filter and its rows carry no claim data, so do not pass `owner`.
3. For each returned task, `daruma_get id=<task id>` (no comments) and keep
   it only when `current_claim.is_mine == true` (equivalently,
   `current_claim.agent_id == <mcp_agent_id>`). A task with
   `current_claim = null` or another agent's claim is not yours.
   Render the kept tasks:

   ```
   ## My active tasks — <N>

   | # | Pri | Title | Claimed | Expires |
   |---|-----|-------|---------|---------|
   | 1 | p1 | <title> | <current_claim.acquired_at, relative, e.g. "2h ago"> | <current_claim.expires_at, relative> |
   …
   ```

4. If the list is empty:

   ```
   No tasks claimed by this session.
   → run /daruma-next to pick up the next ready task.
   ```

5. Read-only. Do not transition any task here.
