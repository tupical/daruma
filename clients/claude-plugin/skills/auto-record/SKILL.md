---
name: auto-record
description: Automatically capture a durable lesson from the current session at stop time. Triggered by the Stop hook when TASKAGENT_ACTIVE_TASK is set.
---

# taskagent-claude: auto-record

The Stop hook fires this skill at the end of every session that had an
active taskagent task (`TASKAGENT_ACTIVE_TASK` is set). It decides whether
a lesson worth keeping was produced and, if so, records it via
`taskagent_comment`.

## Decision gate

Only record when ALL of the following are true:
- A concrete, reusable learning occurred (command, invariant, bug pattern,
  file or API convention).
- The lesson is not already documented in the repo (CLAUDE.md, README,
  inline comments).
- The lesson is likely to be useful in a **future** session on a different
  task or by a different agent.

Skip silently when:
- The session only made ordinary progress with no novel insight.
- The session was read-only (research, review without changes).
- The lesson is too specific to this single task to generalise.

## Step 1 — Identify the target task

Use `TASKAGENT_ACTIVE_TASK` as the task id. If the variable is absent,
fall back to `taskagent_list status=["in_progress"] limit=1` to find the
most-recently updated in-progress task.

## Step 2 — Draft the lesson

One paragraph maximum. Include:
- The concrete thing learned (command, path, flag, invariant).
- Why it matters (what failure or confusion it prevents).
- Optionally: how to apply it.

Prefix the body with `lesson:` so `taskagent_lesson_recall` can surface it.

## Step 3 — Post the comment

```json
{
  "task_id": "<task_id>",
  "body": "lesson: <short durable lesson>"
}
```

Call `taskagent_comment` with the above payload.

## Step 4 — Confirm

Print one line:
```
[auto-record] Lesson captured on <short_task_id>.
```

Then stop — do not re-open the session, do not ask follow-up questions.
