---
description: Show daruma tasks linked to the current git branch.
---

The user invoked `/daruma-claude:branch-tasks`. Read-only.

## Steps

1. Run `git branch --show-current`.
2. If the branch is empty, stop with "no current git branch".
3. Resolve project with `daruma_workspace_info`.
4. Call `daruma_search` with:

   ```json
   {
     "query": "branch:<branch>",
     "scope": "comments",
     "limit": 50
   }
   ```

   Pass the resolved `project_id` when available.
5. Render the matching tasks grouped by task id. Do not change task status.
