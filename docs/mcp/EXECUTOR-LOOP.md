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
| 4 | `daruma_set_status` | `{ id, status: "in_progress", comment: { body, kind: "intent" } }` — the comment lands in the same transaction; no separate `daruma_comment` |
| 5 | *(work)* | Agent edits codebase; no direct DB writes |
| 6 | `daruma_complete` | `{ id: task_id, reason, result_summary, … }` — the completion note replaces a preliminary outcome comment |
| 7 | `daruma_run_finish_step` | `{ run_id, task_id, outcome: { kind: "done" } }` |
| 8 | goto 2 | |
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

## Pass completion contract

Status: **specification before code** (plan
`01a12b83-25e2-7ae2-ba88-7b527c807e1e`, item S1). It fixes what "a pass
finished" means so that run closure, audit findings and launcher readback
(S2–S13) check the same postconditions. It does not accept ADR-0011 and adds
no table, status, service or agent. Core changes start only after an
independent review of this section.

### Current behaviour (baseline the contract tightens)

- `CompleteRun` checks only `run.status == Active`; it reads neither steps nor
  task history and has no owner check (`record_run_terminal` guards only
  `FailRun`/`AbortRun`).
- `RunFinishStep` records any outcome for any `task_id`; `{kind:"done"}` does
  not require a task transition.
- The hosted step-loop detector (ADR-0008) reads only finished steps
  (`finished_at IS NOT NULL`) with failed outcomes, so an open step or a
  falsely successful pass is invisible to it.

### Terms

- **Attempt** — one `RunStepStarted{run_id, task_id}` up to its
  `RunStepFinished` (projection: one `run_steps` row). An attempt starts at the
  step's `started_at`.
- **Handoff transition** — a recorded task status change made after the
  attempt started, into the status this pass is expected to produce (table
  below). Only the history event counts, not a comment, Git commit or the
  model's own report.
- **Step done ≠ task done.** `RunOutcome::Done` on a step means "this attempt
  produced its handoff". The task reaches `done` only through the reviewer
  (or a human), never because a step or run ended `Done`.

### Matrix

| Pass | Task transition proving the attempt | Step outcome | Run end |
|------|------------------------------------|--------------|---------|
| Builder handoff | `in_progress → in_review` after attempt start | `done` | `CompleteRun` |
| Reviewer accept | `in_review → done` after attempt start | `done` | `CompleteRun` |
| Review reject | `in_review → todo` after attempt start, with a reject comment | `done` (the review attempt succeeded) | `CompleteRun` |
| Builder blocked | `in_progress → todo` with a `blocker` comment | `failed{reason}` | `AbortRun` |
| Cancel | task `cancelled` by owner | `superseded` or `failed{reason}` | `AbortRun` / `FailRun` |
| Failed attempt | none required | `failed{reason}` | `FailRun` / `AbortRun` |
| Superseded (task left plan, human completed) | none required | `superseded` / `human_completed` | as today |
| Research / no-task run | none — no step was started | — | `CompleteRun` (compatible) |

A later review reject or reopen does **not** make an earlier builder handoff
false: the check looks for the transition inside the attempt's own window,
not at the task's current status.

### Postconditions

1. **Successful task step.** `RunFinishStep{outcome: done}` is valid only if
   the task history holds the handoff transition for this pass kind recorded
   after this attempt's `started_at`. Otherwise the step must be recorded as
   failed by the caller or rejected by the server.
2. **No open step at completion.** `CompleteRun` fails with a conflict while
   any `run_steps` row of the run has `finished_at IS NULL`.
3. **No-step runs stay compatible.** A run with zero steps (research, plan
   inspection) may still `CompleteRun`. A *launcher* pass that claimed a task
   but recorded no step is unsuccessful: the launcher treats it as failed;
   the server does not reject it, because it cannot tell the two apart.
4. **Idempotent retry.** Repeating `CompleteRun` for a run already
   `Completed` by the same owner returns success without a new event (lost
   response). The same call by a different agent, or on a `Failed`/`Aborted`
   run, stays a conflict.
5. **Pipeline-managed tasks.** For tasks with `metadata_json.pipeline_position`
   the handoff is reached through `AdvancePipeline` and read via
   `canonical_status`; checks never bypass the direct-status guard
   (`direct_status_write_allowed`) to "fix" a status.

### Orphan boundary (signal only)

A run is reported as orphaned (`run.orphaned`, S5) only if it is `Active` and
either

- `last_activity_at` is older than **2 h** *and* every claim it holds expired
  more than **10 min** ago, or
- the launcher process that owns it is proven to have exited (launcher
  readback, S8–S11).

The timer and the audit **only signal**: they never set a task to `done` or
`todo`, never complete a task from a commit or run outcome, and never abort a
run that belongs to another agent. Status drift (`task.status_drift`, S6) is
likewise a finding, not a correction.

### Consistent read

The postcondition reads (open steps, task history after `started_at`, claim
holder) run inside `CommandHandler::handle` under the existing
`plan_run_lifecycle` guard, i.e. the same serialization point that already
orders claims and task status projection, and the resulting run event is
appended in the same handler call. Cross-process ordering keeps the known
limitation documented on that guard (one handler instance); the contract adds
no new lock. Audit checks (S5/S6) read committed projections and tolerate a
transition that lands between their reads by re-checking on the next tick.

### Operating modes

The contract is deterministic: every check reads events and projections and
needs no AI provider. With a provider configured, nothing changes — models
may write richer comments, but success is still decided by the recorded
transitions above.
