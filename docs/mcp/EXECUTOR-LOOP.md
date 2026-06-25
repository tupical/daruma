# Executor loop (MCP)

Canonical agent loop for draining a plan without a human in the loop. Stateless server: the agent drives iteration via MCP tools.

## Prerequisites

- Plan status **`active`** (use `daruma_plan_set_status` if still `draft`).
- A **`run_id`** from `daruma_run_start` (or a fresh UUID per drain pass — see `plan_next_task` docs).
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
next = plan_next_task(plan_id, run_id, claim_ttl_secs=300)
  ↓
set_status(next.task_id, in_progress)
  ↓
claim(next.task_id, agent_id, ttl)     ← optional if next_task already claimed
  ↓
<execute work in repo / run tests / edit files>
  ↓
comment(task_id, body=<artifact summary>, kind=outcome)
  ↓
complete(task_id)
  ↓
run_finish_step(run_id, task_id, outcome={kind:"success"})
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
| 8 | `daruma_run_finish_step` | `{ run_id, task_id, outcome: { kind: "success" } }` |
| 9 | goto 2 | |
| ∞ | `daruma_run_complete` | When plan drained |

## Prompt template (drop into agent system context)

```markdown
You are executing plan {{plan_id}} for project {{project_title}}.

Loop until `daruma_plan_progress` returns no `next_ready` and all tasks are done:

1. Call `daruma_plan_progress` — if `next_ready` is absent and counts show completion, call `daruma_run_complete` and stop.
2. Call `daruma_plan_next_task` with `claim_ttl_secs=300`.
3. Set the task `in_progress`, do the work, leave an `outcome` comment summarizing changes.
4. Call `daruma_complete` and `daruma_run_finish_step` with `{ "kind": "success" }`.
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
| Step failed | `run_finish_step` with `{ "kind": "failure", "reason": "…" }`; optionally `daruma_reopen` after fix. |
| Human interrupt | `daruma_run_abort` + release claims. |

## Related tools (roadmap)

- **`daruma_plan_drain_next`** (M5.2) — atomic `plan_next_task` + `claim` in one call.
- **`daruma_can_start`** (M2.1) — preflight blockers before `set_status(in_progress)`.

## See also

- [../guides/comment-conventions.md](../guides/comment-conventions.md) — `lesson:` prefix
- [../guides/ai-agent.md](../guides/ai-agent.md) — AI layer rules
- `clients/claude-plugin/lib/orchestrator.mjs` — reference implementation using `plan_next_task`
