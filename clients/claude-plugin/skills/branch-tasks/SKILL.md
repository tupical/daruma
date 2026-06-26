---
name: branch-tasks
description: Find daruma tasks associated with the current git branch through `branch:` comments.
---

# daruma-claude: branch-tasks

Use this when work resumes on an existing git branch and the relevant daruma task is not obvious.

## Steps

1. Run `git branch --show-current`.
2. If no branch is checked out, stop; detached HEAD has no branch key.
3. Call `daruma_search` with `query = "branch:<branch>"`, `scope = "comments"`, and `limit = 50`.
4. Show task ids, titles, status, and the matching `branch:` comment snippet.

Do not claim, complete, or modify tasks from this skill.
