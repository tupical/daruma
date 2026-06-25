---
name: lesson-recall
description: Recall previously captured daruma lessons through `daruma_lesson_recall` before repeating similar work or debugging a familiar failure mode.
---

# daruma: lesson-recall

Use this before starting work that resembles a previous task, failure, setup issue, or project convention.

## Step 1 - Search lessons

Call:

```json
{
  "query": "<keywords>",
  "limit": 10
}
```

through `daruma_lesson_recall`. Pass `project_id` or `scope_path` when the workspace has multiple daruma scopes.

## Step 2 - Apply only relevant lessons

Use lessons only when the referenced context matches the current repository and task. If the results are stale or unrelated, ignore them and continue.
