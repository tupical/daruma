---
description: Capture a durable lesson from the current session as a daruma comment.
---

The user invoked `/daruma-claude:capture`.

Record a reusable lesson (command, invariant, bug pattern, file convention)
discovered during this session. The lesson is stored as a `daruma_comment`
with a `lesson:` prefix on the active or most-relevant task.

## When to capture

Capture only when there is a **concrete, reusable** lesson — something that
would save time in a future session. Skip for:
- Ordinary progress updates or temporary guesses.
- Facts already documented in the repository.
- One-off decisions specific to this task only.

## Steps

1. **Identify the target task.**
   - Check `DARUMA_ACTIVE_TASK` environment variable first (set by
     the agent when it claims a task via `daruma_plan_next_task`).
   - If absent: `daruma_workspace_info` → `daruma_list` with
     `status = ["in_progress"]`, pick the most-recently updated task.
   - If still none: ask the user which task to attach the lesson to.

2. **Draft the lesson text.**
   - If the user provided a description, use it verbatim.
   - Otherwise synthesize one sentence from the most significant
     learning in this session (command that worked, invariant discovered,
     failure mode avoided).

3. **Post the comment:**
   ```
   daruma_comment
     task_id = <task_id>
     body    = "lesson: <short durable lesson — one paragraph max>"
   ```

4. **Confirm:**
   ```
   ✅ Lesson recorded on task <short_id>:
   "<lesson text>"
   ```

5. Read-only after posting — do not change task status.
