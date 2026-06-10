# MCP tool-surface profiles

TaskAgent's MCP catalogue grew API-first to ~94 tools. Most agent sessions
use a dozen of them, and a 94-tool `tools/list` burns context before the
user's request is even read. Profiles split the surface:

| Profile | Tools | Audience |
|---------|-------|----------|
| `default` | 31 — compact, workflow-first | Everyday agent work: capture → plan → execute → close |
| `full` | complete catalogue | Power users, orchestrators, dashboards, backward compat |

`full` is always a strict superset of `default`.

## Selecting a profile

Resolution order (first match wins):

1. **Explicit override** — `taskagent mcp --profile full` (stdio) or
   `POST /v1/mcp?profile=full` (HTTP).
2. **Environment** — `TASKAGENT_MCP_PROFILE=default|full` (aliases:
   `core` → default, `all`/`compat` → full).
3. **Built-in default** — `default`.

`tools/list` returns only the selected profile's tools. Tools hidden by the
profile are **not callable**: `tools/call` on a hidden tool returns an error
that names the tool and explains how to enable the full catalogue. Unknown
tool names keep their normal unknown-tool error.

## Migration

Before 0.3 the entire catalogue was always advertised. If your client
depends on advanced tools (history, documents, sessions, workspacegraph,
AI ops, bulk ops), pin the full surface explicitly:

```bash
# stdio (Claude Code, Codex, …)
claude mcp add taskagent -- taskagent mcp --profile full
# or
export TASKAGENT_MCP_PROFILE=full
```

```text
# HTTP (Cursor remote MCP)
url: http://localhost:8080/v1/mcp?profile=full
```

Guarantees:

- `full` keeps every tool name, input schema, and response shape unchanged.
- No tool was renamed or removed as part of profiles.
- There is no single generic `taskagent_call` wrapper and none is planned —
  tools stay individually addressable (non-goal).

## Default profile composition

The `default` profile covers the complete everyday loop with one tool per
job — competing/overlapping alternatives stay in `full`:

| Domain | In `default` | In `full` only (rationale) |
|--------|--------------|----------------------------|
| Tasks | create, capture, get, update, list, search, comment, set_status, set_priority, complete, reopen, can_start | capture_batch, bulk_set_status (bulk = orchestration), split, move_project (rare), delete (destructive), lesson_recall |
| Projects | project_list, project_use, workspace_info, healthz | project_create (rare), project_delete (destructive, two-step), workspace_resolve, workspace_list, project_move_workspace (registry ops) |
| Plans | plan_create, plan_get, plan_list, plan_add_task, plan_set_status, plan_progress, plan_drain_next | plan_update, plan_remove_task, plan_reorder, plan_archive, plan_next_task (superseded by drain_next), plan_graph, plan_fanout, bulk_attach_to_plan |
| Runs | run_start, run_complete, run_abort, run_note_append | run_start_step, run_finish_step, run_log, run_notes_list (step-level tracing) |
| Coordination | claim, release | reserve_files, release_files, active_work, ready, ready_drain, doctor, suggest_files, inbox_pull, work_unit_* (5) (multi-agent orchestration) |
| Relations | link, relations | unlink (destructive) |
| Search/graph | — (plain `search` is in Tasks) | workspacegraph_* (5; competes with list/search for inventory questions) |
| Documents | — | doc_* (7) |
| History | — | history_* (6; incl. destructive rollback) |
| AI | — | ai_parse, ai_decompose, ai_analyze_complexity, ai_scope, research (open-world, costed) |
| Events/admin | healthz | subscribe_project, events_since, sessions (7), signals (2) |

Why each excluded group is `full`-only:

- **WorkspaceGraph / documents / history / sessions / signals / events** —
  introspection and audit surfaces. Valuable, but they compete with the
  one-obvious-tool rule (`list` vs `search` vs `workspacegraph_search`)
  and are almost never needed in a routine coding session.
- **AI tools** — open-world (call an LLM provider), cost money, and the
  calling agent usually *is* the LLM; exposing them by default invites
  recursion an operator didn't ask for.
- **Destructive tools** (`delete`, `project_delete`, `plan_archive`,
  `doc_archive`, `unlink`, `history_rollback`) — explicit opt-in via `full`.
- **Multi-agent coordination** (leases, ready pool, doctor) — only
  meaningful in parallel-agent setups, which are configured deliberately.

## Annotations

Every advertised tool carries a human-readable `title` plus MCP
`ToolAnnotations` (spec 2025-06-18): `readOnlyHint`, `destructiveHint`,
`idempotentHint`, `openWorldHint`. Internally each entry picks one preset
(`Read`, `Write`, `WriteIdem`, `Destructive`, `AiRead`, `AiWrite`) in
`crates/mcp/src/tools.rs` — there is no default, so a new tool cannot skip
the decision.

## Adding a new tool — checklist

1. Add the entry in `tool_definitions()` (`crates/mcp/src/tools.rs`) via the
   `tool(...)` constructor — name, **title**, decision-oriented description,
   schema, **domain**, **profile**, **annotation preset**. All fields are
   mandatory by construction.
2. Profile choice: `default` only if the tool is part of the everyday
   workflow *and* does not compete with an existing default tool for the
   same question. When in doubt → `full`.
3. Description style: one sentence of purpose, then when-to-use; add a
   safety caveat only for genuinely risky calls (destructive, archive-sized
   responses). Don't restate the schema.
4. Add the dispatch arm in `call_tool` and cover the tool in tests
   (`tools::profile_tests` asserts metadata invariants automatically; add
   behavior tests in `apps/server/tests/`).
5. If the tool is destructive or open-world, double-check the annotation
   preset — clients use these hints for confirmation UX.
