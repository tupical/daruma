//! Static MCP prompt catalogue.
//!
//! Source: `clients/cursor-plugin/cursor/commands/*.md`; bodies are copied
//! without YAML frontmatter so MCP clients can fetch the prompts from the
//! server directly.

#[derive(Clone, Debug, serde::Serialize)]
pub struct PromptDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
}

pub fn prompt_definitions() -> Vec<PromptDef> {
    vec![
        PromptDef {
            name: "daruma-tasks",
            title: "Daruma: open tasks",
            description: "Show pending and in-progress tasks from daruma as a compact table.",
        },
        PromptDef {
            name: "daruma-plan",
            title: "Daruma: active plan",
            description: "Show the active plan's checklist with progress bar from daruma.",
        },
        PromptDef {
            name: "daruma-next",
            title: "Daruma: claim next task",
            description:
                "Claim the next ready task from the active daruma plan and show its briefing.",
        },
        PromptDef {
            name: "daruma-mine",
            title: "Daruma: my claimed tasks",
            description: "Show tasks currently claimed by this agent / session.",
        },
    ]
}

pub fn prompt_body(name: &str) -> Option<&'static str> {
    match name {
        "daruma-tasks" => Some(DARUMA_TASKS),
        "daruma-plan" => Some(DARUMA_PLAN),
        "daruma-next" => Some(DARUMA_NEXT),
        "daruma-mine" => Some(DARUMA_MINE),
        _ => None,
    }
}

const DARUMA_TASKS: &str = r#"# /daruma-tasks

Fetch the current task list from the daruma MCP server and render it as
a markdown table.

## Steps

1. Resolve the active project:
   - Call `daruma_workspace_info`. Use `default_project` if present.
   - Otherwise call `daruma_project_list` and pick the first one. If
     none exist, tell the user "no projects yet — create one with
     `daruma_project_create`" and stop.

2. Fetch tasks (filter on the server — do **not** load everything and
   filter locally):
   - `daruma_list` with `project_id = <resolved>`, `status =
     ["inbox", "todo", "in_progress"]`. Limit to ~50.
   - **Never** use `status=all` in this command unless the user explicitly
     asked for the full archive — `all` is token-heavy.

3. Render exactly this format, nothing else:

   ```
   ## <project title> — <N> open tasks

   | # | Status | Pri | Title | Plan |
   |---|--------|-----|-------|------|
   | 1 | 🟢 in_progress | p1 | Wire installCommands into bin/install | plan-xxx |
   | 2 | ⬜ todo | p2 | Add tests for commands.mjs | plan-xxx |
   …
   ```

   Status emoji: 📥 inbox, ⬜ todo, 🟢 in_progress, ✅ done.
   Priority shown as-is (`p0`–`p3`). Title truncated to 60 chars with `…`.
   `Plan` column shows the short plan id (last 8 chars) or `—` if no plan.

4. If there are more than 30 rows, render the first 30 and add a footer:
   `…and <N> more — narrow with /daruma-mine or a status filter`.

5. Do **not** invent IDs, statuses, or counts. If `daruma_list` fails,
   surface the error verbatim and stop.

6. Do not write anything to local plan or markdown task files — this
   command is read-only.
"#;

const DARUMA_PLAN: &str = r#"# /daruma-plan

Render the active execution plan as a markdown checklist with a progress
bar. Use the daruma MCP server.

## Steps

1. Resolve project (`daruma_workspace_info` → `default_project`).
2. `daruma_plan_list` with `project_id = <resolved>`,
   `status = "active"`, `sort = "updated"`, `limit = 1` — the server orders
   by `updated_at`, newest first, so the single row is the most recently
   updated plan (the default order is by creation, and its first page can
   miss it). If none, say "no active plan — `daruma_plan_materialize`
   first" and stop.
3. `daruma_plan_get` with the chosen `plan_id` and `view = "tasks"` —
   the only view that lists every plan task (done included) with
   `status`, `priority`, `depends_on` and `blocked_by`. The default
   `progress` view caps each group at five open tasks; don't use it here.
4. Compute progress: `progress.tasks_done / progress.tasks_total`. Build a 20-cell bar like
   `▓▓▓▓▓▓▓▓░░░░░░░░░░░░ 40%`.
5. Render exactly:

   ```
   ## <plan title>

   Project: <project title>
   Progress: ▓▓▓▓▓▓▓▓░░░░░░░░░░░░ 40%  (4 / 10)
   Plan id:  <plan_id>

   ### Tasks

   - [x] ✅ <title>            — done
   - [ ] 🟢 <title>            — in_progress
   - [ ] ⬜ <title>            — todo (p1)
   - [ ] ⬜ <title>            — todo (p2, blocked-by <other-task-id>)
   …
   ```

   - `[x]` only for `done`; everything else is `[ ]`.
   - Status emoji: ⬜ todo, 🟢 in_progress, 🔍 in_review, ✅ done,
     ❌ cancelled, 📥 inbox.
   - Priority shown only when `!= p2`.
   - `blocked-by` rendered only when `blocked_by` is non-empty (unfinished
     `depends_on` tasks and active `blocks` relations).

6. Below the list, suggest the next action one of these ways:
   - If any `in_progress` task exists → `→ continue: <title>`.
   - Else if any `todo` task is ready → `→ next: run /daruma-next`.
   - Else if all done → `→ plan complete — run daruma_plan_set_status status=completed`.

7. Read-only — never modify tasks here. Don't touch local or
   markdown plan files.
"#;

const DARUMA_NEXT: &str = r#"# /daruma-next

Claim the next ready task from the active plan, set it to `in_progress`,
and render a compact briefing for the user.

## Steps

1. Resolve project (`daruma_workspace_info` → `default_project`).
2. Find the active plan: `daruma_plan_list` with `status = "active"`,
   `sort = "updated"`, `limit = 1` — the most recently updated plan.
   If none, stop with "no active plan — `daruma_plan_materialize` first".
3. Claim next: `daruma_plan_drain_next` with `plan_id`. The server
   atomically claims the next ready (unblocked) task for this session and
   returns null when nothing is ready. Keep the returned `run_id`: the server
   opened that run for this claim. A null drain already aborted its own run —
   nothing to close.
4. If the server returns "no ready task" (plan empty or all blocked),
   render:

   ```
   No ready task. <reason from server, e.g. "3 tasks blocked by X">
   → run /daruma-plan to inspect dependencies.
   ```

   Stop.

5. Otherwise render the briefing:

   ```
   ## Next task: <title>

   id:        <task_id>
   plan:      <plan_id>
   run:       <run_id>
   priority:  <pX>
   status:    🟢 in_progress
   ```

   Then a `### Description` section with the task description verbatim
   (unwrapped), and if non-empty:

   ```
   ### Dependencies
   - <dep_task_id> — <dep_title> (status)
   ```

   ```
   ### Related (links)
   - <kind>: <related_task_id> — <title>
   ```

6. Finish with a short call-to-action:

   ```
   → When done: daruma_complete task_id=<task_id> [comment="<summary>"]
     then daruma_run_complete run_id=<run_id>
   → On failure: daruma_comment task_id=<task_id> body=<reason>
     followed by daruma_set_status task_id=<task_id> status=todo
     and daruma_run_abort run_id=<run_id> reason=<reason>
   ```

   An open run keeps the plan from auto-closing after its last task; the
   `plan_close_blocked_by_run` warning names it in `details.run_id`.

7. Do not start executing the task yourself in this command — this is a
   briefing only. The user (or a follow-up agent prompt) drives execution.
"#;

const DARUMA_MINE: &str = r#"# /daruma-mine

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
"#;
