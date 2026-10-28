# Executor loop (MCP)

Canonical agent loop for draining a plan without a human in the loop. Stateless server: the agent drives iteration via MCP tools.

## Prerequisites

- Plan status **`active`** (use `daruma_plan_set_status` if still `draft`).
- A **`run_id`** returned by `daruma_run_start`, or omit it when calling `daruma_plan_drain_next` so the server starts an authenticated run. Never invent a run UUID.
  An omitted-`run_id` drain that hands out no task (null or error) aborts the run it opened itself; one that hands out a task returns that `run_id` — keep it, and after the task's handoff close it with `daruma_run_complete` (or `daruma_run_abort`), otherwise it holds the plan open (`plan_close_blocked_by_run` names it).
- Workspace default project set (`daruma_project_use`) when tasks are project-scoped.

## Loop

```text
run_start(plan_id)                     ← agent_id defaults to the token's agent
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
run_start_step(run_id, task_id)        ← opens the attempt; before the work
  ↓                                       and before any status transition
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
| 1 | `daruma_run_start` | `{ plan_id }` — `agent_id` is optional and defaults to the authenticated agent |
| 2 | `daruma_plan_progress` | Stop when `next_ready` is null and `todo + in_progress == 0` |
| 3 | `daruma_plan_next_task` | `{ id: plan_id, run_id, claim_ttl_secs: 300 }` — `plan_id` is accepted for `id`; `run_id` is optional (server-generated) |
| 4 | `daruma_run_start_step` | `{ run_id, task_id }` — opens the attempt **before** the work and before any status transition of this pass (see below) |
| 5 | `daruma_set_status` | `{ id, status: "in_progress", comment: { body, kind: "intent" } }` — the comment lands in the same transaction; no separate `daruma_comment` |
| 6 | *(work)* | Agent edits codebase; no direct DB writes |
| 7 | `daruma_complete` | `{ id: task_id, reason, result_summary, … }` — the completion note replaces a preliminary outcome comment |
| 8 | `daruma_run_finish_step` | `{ run_id, task_id, outcome: { kind: "done" } }` |
| 9 | goto 2 | |
| ∞ | `daruma_run_complete` | When plan drained |

Step 4 is not optional. `run_finish_step` without an open step for that task
records an attempt that starts at the finish timestamp, so the `complete` of
step 7 lies before the attempt and `run_complete` rejects the run with 409
`run_postcondition_unmet` ([Pass completion contract](#pass-completion-contract)).
`run_start_step` and `run_finish_step` are in the `full` profile
([PROFILES.md](PROFILES.md)).

## Implicit status transitions

Status follows what the agent did, so a forgotten `set_status` does not leave
work invisible:

| Write | From | To |
|-------|------|----|
| `daruma_claim`, `daruma_comment kind=intent\|progress`, `daruma_run_start_step`, task-scoped evidence, `git_context` attached via `daruma_update` | `todo` | `in_progress` |
| `daruma_comment kind=outcome` | `todo`, `in_progress` | `in_review` |

`kind=blocker`, `kind=research` and plain comments move nothing; neither does
any write on another status (`inbox`, `in_review`, terminal). The move is the
same `SetStatus … force` as drain: relation blockers are acknowledged,
lifecycle rules (`task.before_start`, …) still apply. When a rule blocks it,
the write itself succeeds, the status stays and the response carries a
warning `implicit_status_blocked` with the rule message. Callers without
`TaskWrite` never move status this way. `todo → done` stays allowed. An
expired claim never changes status: the claim TTL sweep only removes the
claim, and the stall is reported as a claim-expired signal.

`daruma_claim`, `daruma_release` and `daruma_run_start` take `agent_id` as
optional — the server uses the authenticated agent (`DELETE
/v1/claims/me/{task_id}` releases the caller's own claim).

## Lifecycle hints in results

Task mutations answer with where the task is now and which lifecycle step
follows, as data (`next_tool: {name, arguments}` or `null`; the client
decides whether to call it):

| Tool | Added fields | `next_tool` |
|------|--------------|-------------|
| `daruma_plan_materialize` | `task_ids` (plan order) | `daruma_plan_drain_next {plan_id}` |
| `daruma_plan_drain_next`, `daruma_claim` | `task {id, status, claim_expires_at?}` | `daruma_comment {task_id, kind: outcome}` |
| `daruma_comment kind=outcome` | `task {…}` | `daruma_complete {id}` while the task is open, else `null` |
| `daruma_complete` | `task {…}` | `null` |

`task` is read back after the mutation (`claim_expires_at` only for the
caller's own claim); when the read fails, `task` is absent and the
comment's `next_tool` is `null` — the mutation result itself is never lost.
Other comment kinds carry no hint. `verbose: true` returns the raw body
without hints.

`initialize.instructions` ends with `Open tasks of this agent: <id> —
"<title>" — <status>; …` when the token's agent holds live claims on
`in_progress`/`in_review` tasks (at most 5, titles cut to 40 chars, quoted).
The whole text stays within 2048 chars: entries that do not fit become
`+N more` (full list: `daruma_list status=active`). The same tasks ride
`initialize._meta["daruma/open_tasks"]`, so a hosting gateway that appends
its own rules re-renders the line after them within the same ceiling. The
lookup is bounded (2 s, `OPEN_TASKS_LOOKUP_TIMEOUT`) on both paths — the
stdio process reading over HTTP and a hosting server reading in-process; past
it or on any failure the line is simply absent and `initialize` still
succeeds.

## Prompt template (drop into agent system context)

```markdown
You are executing plan {{plan_id}} for project {{project_title}}.

Loop until `daruma_plan_progress` returns no `next_ready` and all tasks are done:

1. Call `daruma_plan_progress` — if `next_ready` is absent and counts show completion, call `daruma_run_complete` and stop.
2. Call `daruma_plan_next_task` with `claim_ttl_secs=300`.
3. Call `daruma_run_start_step` with `{ run_id, task_id }` before any work or status change.
4. Set the task `in_progress`, do the work, leave an `outcome` comment summarizing changes.
5. Call `daruma_complete` and `daruma_run_finish_step` with `{ "kind": "done" }`.
6. On blocker: comment with `kind=blocker`, do not complete; move to the next ready task or stop and report.

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
| `run_complete` → 409 `run_postcondition_unmet` | A step is still open, or a step finished `done` without a handoff transition since it started. Record the transition (or finish the step `failed`) — see [Pass completion contract](#pass-completion-contract). A `run_finish_step` without a preceding `run_start_step` starts the attempt at the finish time, so a handoff made earlier is not counted; start the step before the work. |
| `run_complete` → 403 `run is owned by another agent` | The run's owner (`run_claim_owners`) is another agent. Only the owner or an admin completes an active owned run; finish through the token that opened it. |
| Response of `run_complete` / `run_finish_step` lost | Repeat the same call with the same token. Success with `data.already_completed` / `data.already_finished = true` and `event_id: null` means the first call landed — see [Idempotent retry](#idempotent-retry-s4). |

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
  `FailRun`/`AbortRun`). S3 added the step postconditions and S4 the owner
  retry no-op; an active run's completion still has no owner check.
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

### Step projection in core

Core reads attempts through `RunRepository` over the existing `run_steps`
projection (no extra table): `list_steps(run_id)` returns the steps in start
order (`started_at`, then `id`), and `current_step_task(run_id)` returns the
task of the latest step with `finished_at IS NULL` in that order, or `None`
when the run has no open step. A repeated attempt of the same task is a new
row; `RunStepFinished` closes the latest open row of that task. Because the
current step is now real, `RemovePlanTask` on a task that is an active run's
open step emits `TaskContested` + `RunStepFinished{superseded}` (§3.7) instead
of silently skipping it.

### Enforcement in `CompleteRun` (S3)

Postconditions 1–3 below are enforced by `CompleteRun` (task
`01a12b83-25e4-7072-830d-2e6fc816be77`). Before `RunCompleted` is built the
handler reads `list_steps(run_id)` and, for every step finished `{kind:"done"}`,
the task's `activity` history at or after the step's `started_at`. It rejects
with a conflict `run_postcondition_unmet: run <run_id> …task <task_id>…` (HTTP
409) — and appends nothing — when

- a step is still open (`finished_at IS NULL`), or
- a `done` step has no handoff transition since it started: a status change
  into `in_review` or `done`, a review reject `in_review → todo`, or (for a
  pipeline-managed task) an `advance_pipeline` position change, which may keep
  the canonical status unchanged (draft → builder).

The window has only a lower bound, so a transition recorded after
`run_finish_step` but before `run_complete` (the usual launcher order) counts,
and a later REVIEW REJECT or reopen does not erase it. Steps finished with any
other outcome (`failed`, `superseded`, `human_completed`) and runs with no
steps complete as before. The check reads committed projections, not
in-process state, so it gives the same answer through any handler instance
serving the workspace (hosted tenant cache, cabinet bus); a transition not yet
committed makes it reject, never accept, and the caller retries. A
`run_start_step` racing a `run_complete` through *another* instance is not
excluded by this read; S5 (`run.orphaned`) reports such a step.

### Idempotent retry (S4)

Postcondition 4 below is enforced by `CompleteRun` and `RunFinishStep` (task
`01a12b83-25e4-7072-830d-2e71a3c8a139`), so a launcher that lost a response
repeats the call instead of guessing:

- `POST /v1/runs/{id}/complete` (`daruma_run_complete`) is dispatched with
  the token identity. On a run already `Completed` whose owner
  (`run_claim_owners`, written by the owned `run_start` / `drain_next`) is the
  caller, it returns `200` with `data: {run_id, already_completed: true}`,
  `event_id: null`, and appends nothing; the postconditions are not
  re-evaluated. A first, real completion returns `already_completed: false`
  and the `RunCompleted` event id.
- Any other agent (admin included), an unauthenticated caller, and a
  `Failed`/`Aborted` run still get `409 run is not active`; failed/aborted
  never turn into completed.
- A first completion of an *active* owned run by another non-admin agent is
  refused with `403 run is owned by another agent` and changes nothing
  (docs/PLAN_RECONCILIATION.md). A run without a recorded owner (in-process
  start, or rows before migration 0054) proves nobody's ownership: like
  fail/abort, a non-admin gets `403 run has no authenticated owner` and only
  an admin completes it. Raw run commands over `/v1/commands` or WS dispatch
  are refused (`403`), so every network completion is token-checked.
- `POST /v1/runs/{id}/step/finish` (`daruma_run_finish_step`) is a no-op when
  the task's latest attempt in the run is already closed with exactly the
  requested outcome: `data.already_finished: true`, `event_id: null`, no
  synthetic attempt row. A finish without an open attempt and with a
  different outcome keeps the recorded behaviour (a closed synthetic row).

Launcher readback (S8) treats `success: true` with or without `already_*` as
the same postcondition and still reads the run and task back.

### Launcher readback (S8)

The cron launchers (`~/.hermes/scripts/mcpbox_*.sh`) reach the tracker through
one shared helper, `~/.hermes/scripts/mcpbox_tracker.py` (task
`01a12b83-25e5-70a2-be19-23d8ef5122ac`; tests in
`~/.hermes/scripts/tests/test_mcpbox_tracker.py`). The draft readback
and the builder handoff check (S9, below) use it; the reviewer check (S10)
builds on it.

- **Exit code 0 is not success.** A call succeeds only when stdout parses as
  JSON with no error: a non-zero exit, a timeout, empty or unparseable stdout
  (a lost response), a JSON-RPC `error`, `isError` / `result.isError`, an
  `error` key in the mcporter envelope (`{server, tool, error, issue}`, which
  mcporter may print with exit 0) and `success: false` are all failures,
  classified by kind.
- **Retries.** `429` (rate limited) and timeout / transport / `5xx` / lost
  response are retried with bounded pauses of 3, 6, 15 and 30 s, then the call
  fails. Validation errors, `401`, `403` and other tool errors (e.g. `409`)
  are never retried.
- **Ambiguous mutation.** When a mutating call may have executed (timeout,
  transport, `5xx`, lost response), the helper first runs the caller's
  readback; if it shows the effect, the call counts as done and is not
  repeated. A mutation without a readback is not repeated blindly — it fails.
  `429` means the request was not executed, so it is retried directly.
- **Identifiers.** `task_id`, `run_id` and `claim.agent_id` come only from the
  structured claim response (`drain_next` / `current_claim`), never from a
  title or prose. They are verified by `daruma_get`: the task must read back
  `in_progress` with the same `current_claim` agent, run and claim. A claim
  response without a task or run id is incomplete — the pass is failed, not
  successful.
- **Secrets.** The helper never reads token files; error text is redacted
  (`MCPBOX_STATIC_PAT`, `Bearer …`, `token=…`) before it is logged.

The helper is deterministic and needs no AI provider; with a provider the
models drive the same calls, but the launcher decides success only from these
checks.

### Builder handoff postconditions (S9)

`mcpbox_autonomous.sh` decides the outcome of a builder pass outside the model
(task `01a12b83-25e5-70a2-be19-23ee076229df`; `verify-builder` in
`mcpbox_tracker.py`, scenarios in `tests/test_mcpbox_cycle.py`). The
`RESULT_CLAIMED_TITLE` / `RESULT_NO_CLAIM` marker is only the model's claim;
it no longer makes a pass successful on its own.

- **Window.** Under the repository lock, before the model starts, the launcher
  finds the head of `daruma_events_since` (gallop + bisection from the hint in
  `last.seq`, then one page) and stores it. If the event log is unreachable
  the model is not started and nothing is claimed.
- **IDs from the server.** After the engine exits, `run_started` and
  `agent_claimed` events of the window are filtered by the PAT's
  `mcp_agent_id` (`daruma_workspace_info`). Exactly one task and one run is a
  pass; task, run and claim ids come from those events, the title from
  `daruma_get`.
- **Postconditions** (all must hold): the task reads back `in_review` or
  `done` — or the window shows this pass's handoff followed by a later
  transition out of `in_review` (a review verdict); the handoff transition is
  in the window; a pipeline-managed task (`pipeline_position`) moved through
  `AdvancePipeline` (`task_updated.pipeline_position` right before the status
  change), not a direct status write; the run is `completed` with no open
  step; no claim is held; `work/<task-id>` exists on `origin` and the local
  branch equals it; the checkout is clean; if the branch moves the
  `vendor/oss` pin, that SHA is the head of `work/<task-id>` or `main` in the
  vendor origin.
- **Failure** prints `Builder pass INCOMPLETE` with the failed conditions and
  the task/run/claim ids, exit 1. A tracker error during verification prints
  `UNVERIFIED`, exit 1.
- **Safe repeat.** Only an already declared handoff is finished: when the task
  is already `in_review`/`done` by a transition of this pass, the launcher
  closes the open step with `done`, releases its claim and completes the run
  (each a mutation with readback, S8), then re-checks. It never changes task
  status and never sets `done`.
- **NO_CLAIM** is accepted only when the window has no run and no claim of
  this agent; `RESULT_NO_CLAIM` with a created run/claim, or a claim marker
  without one on the server, is incomplete.

Both operating modes run the same deterministic checks; an AI provider only
drives the model that produces the pass.

### Reviewer verdict postconditions (S10)

`mcpbox_review.sh` decides the outcome of a review pass outside the model
(task `01a12b83-25e5-70a2-be19-23f033bd0465`; `verify-review` in
`mcpbox_tracker.py`, scenarios in `tests/test_mcpbox_cycle.py`). A non-empty
log is no longer a success, and the final marker
`REVIEW_RESULT: accept|reject|escalate <task-id> [root=<sha>] [vendor/oss=<sha>]`
is only the model's claim.

- **Window.** Same as S9: under the lock, before the model starts, the
  launcher stores the head of `daruma_events_since` (hint in the review log
  directory's `last.seq`). An unreachable event log means the model is not
  started.
- **Reviewed task.** The task moved out of `in_review` in the window, or the
  task named by the marker; exactly one is allowed. Without a marker the
  verdict is inferred from the server transition; a marker contradicting the
  transition is incomplete.
- **Accept** (all must hold): the task reads back `done`; the window holds
  `in_review → done`; a comment starting with `REVIEW ACCEPT` was added in the
  window; `work/<task-id>` exists on the root `origin` and is an ancestor of
  the published `origin/main` (read with `ls-remote`), and every claimed SHA is
  too; local `main` is not ahead of `origin/main`; the checkout is clean. When
  root `origin/main` pins `vendor/oss`: the pin is on the vendor
  `origin/main`; a vendor `work/<task-id>` and a claimed vendor SHA are merged
  into the vendor `origin/main` *and* covered by the pin (otherwise *parent pin
  missing*); if the task touched vendor, the pin equals the vendor
  `origin/main` (the `chore(oss): pin reviewed merge` commit); vendor local
  `main` is published and its checkout is clean.
- **Reject**: the task reads back `todo`, the window holds `in_review → todo`
  and a comment starting with `REVIEW REJECT` (the persisted blocker).
  **Escalate**: `inbox` by readback and transition. Both require clean
  checkouts.
- **Queue empty** (`REVIEW: очередь пуста`) is accepted only when the pass
  moved nothing out of `in_review` and `daruma_list status=in_review` is empty;
  the launcher then exits 0 silently (nothing to deliver).
- **Merge without readback** — pushed merge but `set_status` rejected (for
  example a 400 on an oversized comment) or never called — is `REVIEW
  INCOMPLETE`, exit 1. A lost response to the model does not matter: the
  server state decides. A tracker error during verification prints
  `REVIEW UNVERIFIED`, exit 1.
- **Atomic transition comment.** The prompt splits the full verdict (a
  separate `daruma_comment`, any length) from the status transition, whose
  `comment` is omitted or at most 4096 UTF-8 bytes. A validation error on
  `set_status` means the status did not change; the reviewer reads the task
  back and repeats with a short comment.

The launcher never mutates the tracker to decide a verdict and touches git only
with `git fetch` of the refs it checks; the only tracker writes are the S11
recovery of an abandoned run (normally none: review passes open no run). Both
operating modes run the same deterministic checks.

### Abandoned pass recovery (S11)

Both launchers close a pass whose process ended without a handoff (task
`01a12b83-25e5-70a2-be19-2404d2524358`; `pending-write` / `recover-pass` in
`mcpbox_tracker.py`, scenarios in `tests/test_mcpbox_cycle.py`).

- **Pending window.** Right after the window head is stored, the launcher
  writes `pending.json` in its log directory: role, `start_seq`, its own pid
  and that pid's start time, the model log. The file is removed only when the
  outcome is final (verified pass, or recovery finished).
- **Finally and trap.** The model runs as a child under `wait`, so `TERM`,
  `INT` and `HUP` reach the launcher's trap at once (bash defers a trap until a
  foreground child exits). The trap stops the child, then runs the same path as
  a normal exit. Verification (S9/S10) runs for every exit code, including
  `124` and a network failure; a reviewer verdict proven by the server is a
  success even if the engine timed out afterwards. When verification fails,
  `recover-pass --owner-exited` runs on the window: the child has exited, so
  the owner is proven dead.
- **Next preflight.** `SIGKILL` and `oom-kill` cannot be trapped. A
  `pending.json` that survives is handled before the smoke check and before the
  new window head: under the lock, a launcher pid that is gone (or reused with
  another start time) is proven dead. A live owner — or a tracker error — keeps
  the file and stops the pass (`Preflight INCOMPLETE`, nothing claimed).
- **What recovery does.** Only to `Active` runs whose `run_started` in the
  window carries this PAT's `mcp_agent_id`, for each task of the run's open
  steps (or of the pass's claims when no step was started):
  - task `in_review`/`done` with this pass's handoff transition in the window —
    the S9 safe repeat (step `done`, release, `CompleteRun`);
  - otherwise one `blocker` comment `LAUNCHER BLOCKER: launcher_abandoned
    run=<run_id>` with the log path and the git state (`work/<task-id>` on
    origin, dirty file count), the open step finished
    `{kind:"failed", reason:"launcher_abandoned"}`, the task moved
    `in_progress → todo` (no other status is ever changed, `done` is never
    reopened), then `AbortRun{reason:"launcher_abandoned"}`, which releases the
    run's claims. No new status is introduced.
- **No bypass of rights.** A fresh claim of another agent on the task, a run
  of another agent on the pass's tasks, or a `403`/validation refusal yields
  only a `LAUNCHER_FINDING` comment (kind `progress`) and an explicit
  incomplete result (`Recovery INCOMPLETE` / exit 1); no `force`, no
  `override_reason`, no retry of the refused call. An expired foreign claim
  does not block recovery.
- **Repeat is a no-op.** Every write is checked against the window first
  (comment key, open step count, task status, run state) and is a mutation with
  readback (S8), so a repeated recovery after a lost response adds nothing.
- **Limit.** The window has no upper bound: the PAT agent must be used only by
  the launchers under `mcpbox-repo.lock`, otherwise an interactive run started
  inside a stale window could be taken for the pass's own.

Both operating modes behave the same: recovery reads events and projections and
needs no AI provider.

### Fault-injection check and rollout (S13)

One corporate-game scenario checks the whole contract end to end (task
`01a12b83-25e5-70a2-be19-242ee941fb36`; `crates/cloud-api/tests/it/fault_injection_scenario.rs`
in the hosted repository, run with `cargo test -p cloud-api --test it
fault_injection_scenario:: -- --nocapture` and `TEST_DATABASE_URL`). Five
builder passes go through `/v1/mcp`, the 15-minute incident pass and
`/v1/audit/run`, one fault each:

| Fault | Expected result |
|-------|-----------------|
| Handoff `set_status` never called | `run_complete` → `409 run_postcondition_unmet`; run `active`, task `in_progress` until the real handoff |
| Handoff refused (`400`: comment over 4 KiB; `daruma_comment` with `id` instead of `task_id`) | status unchanged; `run_complete` refused as above |
| Mutations applied, responses lost and repeated | `already_finished` / `already_completed: true`, one `run_completed` |
| Builder killed mid-step | two ticks: one open `run.orphaned`, one `[Audit]` task naming the run; the owner's `run_abort` resolves it on the next tick |
| Legitimate REVIEW REJECT after a handoff | task `todo`, no `task.status_drift` (also past the 10-minute grace) |

No tick or audit changes a task status, and tasks of another project keep
status and `updated_at`. The scenario runs without an AI provider and with a
fake one (a counting stub); the outcome is identical, and the stub gets no
call from the passes or the ticks (only the full audit's AI checks may ask
it). The launcher side (S8–S11: lost response, `400` after merge, missing
handoff, abandoned pass) is covered by `~/.hermes/scripts/tests/`.

Rollout:

1. **Observe on old runs.** Runs completed before the S3 server are never
   re-checked by `CompleteRun`; they are only read by `run.orphaned` and
   `task.status_drift`, which signal in `[Audit]` and repair nothing.
   Historical statuses are corrected by a human per finding, outside this
   contract.
2. **Hard gate for new runs.** The `CompleteRun` postconditions (S3) and the
   idempotent retry (S4) apply to every completion after the deploy; there is
   no switch to observe-only. A launcher that sees `run_postcondition_unmet`
   hands off (or fails the step) and repeats the call.
3. **Order.** Deploy the server only after the independent review of this
   plan; then watch the first ticks' `[Audit]` reports for false positives
   before relying on the launcher checks (S8–S11), which are already live and
   decide success from server state only.
4. **Not in scope.** Production deploy and the repair of historical statuses.

### Matrix

| Pass | Task transition proving the attempt | Step outcome | Run end |
|------|------------------------------------|--------------|---------|
| Builder handoff | `in_progress → in_review` after attempt start | `done` | `CompleteRun` |
| Reviewer accept | `in_review → done` after attempt start | `done` | `CompleteRun` |
| Review reject | `in_review → todo` after attempt start, with a reject comment | `done` (the review attempt succeeded) | `CompleteRun` |
| Builder blocked | `in_progress → todo` with a `blocker` comment | `failed{reason}` | `AbortRun` |
| Abandoned pass (launcher, S11) | `in_progress → todo` with a `launcher_abandoned` blocker; other statuses untouched | `failed{launcher_abandoned}` | `AbortRun{launcher_abandoned}` |
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
