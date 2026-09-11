# Executor loop (MCP)

Canonical agent loop for draining a plan without a human in the loop. Stateless server: the agent drives iteration via MCP tools.

## Prerequisites

- Plan status **`active`** (use `daruma_plan_set_status` if still `draft`).
- A **`run_id`** returned by `daruma_run_start`, or omit it when calling `daruma_plan_drain_next` so the server starts an authenticated run. Never invent a run UUID.
- Workspace default project set (`daruma_project_use`) when tasks are project-scoped.

## Loop

```text
run_start(plan_id, agent_id)
  ↓
┌──────────────────────────────────────┐
│  progress = plan_progress(plan_id)   │  ← cheap snapshot
│  if progress.next_ready is null:     │
│      run_complete(run) → EXIT        │
└──────────────────────────────────────┘
  ↓
next = plan_drain_next(plan_id, run_id, claim_ttl_secs=300)
  ↓
<server claims task and sets in_progress>
  ↓
<execute work in repo / run tests / edit files>
  ↓
comment(task_id, body=<artifact summary>, kind=outcome)
  ↓
complete(task_id)
  ↓
run_finish_step(run_id, task_id, outcome={kind:"done"})
  ↓
repeat from plan_progress
```

## MCP tool sequence (minimal)

| Step | Tool | Notes |
|------|------|-------|
| 1 | `daruma_run_start` | `{ plan_id, agent_id }` |
| 2 | `daruma_plan_progress` | Stop when `next_ready` is null and `todo + in_progress == 0` |
| 3 | `daruma_plan_next_task` | `{ id: plan_id, run_id, claim_ttl_secs: 300 }` |
| 4 | `daruma_set_status` | `{ id, status: "in_progress" }` |
| 5 | *(work)* | Agent edits codebase; no direct DB writes |
| 6 | `daruma_comment` | `{ task_id, body, kind: "outcome" }` |
| 7 | `daruma_complete` | `{ id: task_id }` |
| 8 | `daruma_run_finish_step` | `{ run_id, task_id, outcome: { kind: "done" } }` |
| 9 | goto 2 | |
| ∞ | `daruma_run_complete` | When plan drained |

## Prompt template (drop into agent system context)

```markdown
You are executing plan {{plan_id}} for project {{project_title}}.

Loop until `daruma_plan_progress` returns no `next_ready` and all tasks are done:

1. Call `daruma_plan_progress` — if `next_ready` is absent and counts show completion, call `daruma_run_complete` and stop.
2. Call `daruma_plan_next_task` with `claim_ttl_secs=300`.
3. Set the task `in_progress`, do the work, leave an `outcome` comment summarizing changes.
4. Call `daruma_complete` and `daruma_run_finish_step` with `{ "kind": "done" }`.
5. On blocker: comment with `kind=blocker`, do not complete; move to the next ready task or stop and report.

Rules:
- Never skip dependency order — trust `plan_next_task`.
- Prefer small diffs; update docs when behavior changes.
- Record post-mortems as `lesson: …` in comment body (see docs/guides/comment-conventions.md).
```

## Error handling

| Situation | Action |
|-----------|--------|
| `plan_next_task` returns null but tasks remain | Check plan status; verify blockers via `daruma_relations` (`blocks` edges). |
| Claim expired | Re-call `plan_next_task` with fresh TTL or `daruma_claim`. |
| Step failed | `run_finish_step` with `{ "kind": "failed", "reason": "…" }`; optionally `daruma_reopen` after fix. |
| Human interrupt | `daruma_run_abort` + release claims. |

## Related tools

- **`daruma_plan_drain_next`** — atomic `plan_next_task` + `claim` in one call (default profile).
- **`daruma_can_start`** — preflight blockers before `set_status(in_progress)` (default profile).

## See also

- [../guides/comment-conventions.md](../guides/comment-conventions.md) — `lesson:` prefix
- [../guides/ai-agent.md](../guides/ai-agent.md) — AI layer rules
- `clients/claude-plugin/lib/orchestrator.mjs` — reference implementation using `plan_next_task`

## CLI execution bounds

The `daruma-claude` reference executor shares a 100-attempt budget across a plan.
A task stops after two identical nonempty failed-task error/result messages,
three failed executions, an executor exception, or its configured retry limit.
Partial team completion counts do not reset failed-attempt counts: they are not
proof that a build or test regression was fixed. Each stop records a blocker
comment; claimed tasks are released and the plan run is aborted instead of
starting another wave. Plans use the UUID returned by `daruma_run_start`.

These are CLI invocation bounds, not a server-wide token budget or a sandbox
for arbitrary agent side effects. Restarting the CLI starts a new run; the
blocker record remains in Daruma for the next operator's decision.

## Continuation context

Before every CLI execution attempt, Daruma supplies the current task and the
current run's journal (`daruma_run_notes_list`). The CLI's private MCP subprocess
uses the full profile for this read; the default tool catalogue stays compact.
The prompt carries task id/status/version time, run id and up to ten journal
entries with author and timestamp; bodies are bounded to 600 characters.
The read is capped at 500 notes and marks omitted entries/bodies; a full page
indicates that additional server notes may exist.
Every attempt appends a summary to the same server journal. A terminal task
stops retries instead of being reopened from a stale local snapshot.
This projection creates no `consensus.md` or other local state file and does
not guess associations to other AgentSessions sharing an agent id.


## Terminal tasks and atomic claims

`next-task`, plan fanout and project ready/drain exclude `done` and `cancelled`
tasks. Explicit dependency success still requires `done`: cancelling a prerequisite
does not assert that its deliverable exists. `can_start` returns `terminal_task`
for a terminal target; reopen deliberately before attempting a new claim.

HTTP `/claims` and run-bound drain use the same recorded claim transaction.
A missing task returns 404, a terminal task 409. A failed audit append rolls back
the holder, preventing invisible claims. Claim acquisition also shares the handler
lifecycle lock with task status projection, so a completed/cancelled task cannot be
reclaimed in that handler's event-to-projection interval.
