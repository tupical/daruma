---
description: Mark one or more taskagent tasks as done (or cancelled).
---

The user invoked `/taskagent-claude:close [<task_id> ...] [--cancel] [--comment "<text>"]`.

## Flags

- `--cancel` — mark as `cancelled` instead of `done`.
- `--comment "<text>"` — attach a closing comment before transitioning.

## Steps

### With explicit task IDs

1. For each `<task_id>` (short suffixes resolved via `taskagent_search`):
   a. `taskagent_get task_id=<id>` — confirm it exists and is not already closed.
   b. If `--comment` was given: `taskagent_comment task_id=<id> body="<text>"`.
   c. `taskagent_complete task_id=<id>` (or `taskagent_set_status … status=cancelled`
      if `--cancel`).
   d. Print: `✅ Closed <short_id>: <title>`

2. After all IDs: print a one-line summary.
   `Closed <N> task(s).`

### Without arguments — interactive close

1. Resolve project via `taskagent_workspace_info`.
2. `taskagent_list status=["in_progress","in_review"] project_id=<id> limit=20`.
3. If 0 items: `No in-progress tasks to close.` and stop.
4. Render a numbered list:

   ```
   In-progress tasks:
   1. [p1] <title> (<short_id>)
   2. [p2] <title> (<short_id>)
   …
   ```

5. Ask: `Which task(s) to close? Enter numbers, IDs, or "all". (ctrl-c to cancel)`
6. Close the selected tasks as above.

## Guard rails

- Never close a task that is already `done` or `cancelled` — print a notice
  and skip it.
- Never close a task that has unresolved blockers unless the user confirms.
- Never use `status=all` for listing — keep to `in_progress` / `in_review`.
- After closing, suggest `/taskagent-claude:next` if there are remaining
  open tasks in the plan.
