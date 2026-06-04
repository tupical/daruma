---
name: lesson-capture
description: Capture a durable lesson from the current work as a taskagent comment prefixed with `lesson:`. Use at the end of a session when a reusable workflow, bug pattern, command, or project convention was learned.
---

# taskagent: lesson-capture

Record only lessons that are likely to be useful in a later session. Do not capture ordinary progress updates, temporary guesses, or facts already documented in the repository.

## Step 1 - Choose the target task

Prefer the active taskagent task for the current work. If there is no active task, use the task most closely related to the lesson.

## Step 2 - Write the lesson

Call `taskagent_comment` with:

```json
{
  "task_id": "<task_id>",
  "body": "lesson: <short durable lesson>"
}
```

Keep the body one paragraph. Include the command, file path, invariant, or failure mode that makes the lesson searchable.

## Stop auto-lesson

At session stop, run this capture only when there is a concrete reusable lesson. Skip it when the session produced no durable learning.
