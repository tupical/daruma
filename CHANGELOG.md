# Changelog

This file tracks public, user-visible changes for TaskAgent releases.

For the current pre-release history in Russian, see [CHANGELOG.ru.md](CHANGELOG.ru.md).

## Unreleased

### MCP tool-surface profiles

`tools/list` is now profile-gated: the new `default` profile advertises a
compact, workflow-first surface of 31 tools; `full` keeps the complete
catalogue (~94 tools) unchanged. Select with `taskagent mcp --profile`,
`TASKAGENT_MCP_PROFILE`, or `/v1/mcp?profile=`. **Unset now means
`default`** — clients that depend on advanced tools (history, documents,
sessions, workspacegraph, AI ops, bulk ops) must opt into `full`; hidden
tools return an actionable error instead of dispatching. See
[docs/mcp/PROFILES.md](docs/mcp/PROFILES.md) for composition and migration.

- Every advertised tool now carries a human-readable `title` and MCP
  `ToolAnnotations` (`readOnlyHint`, `destructiveHint`, `idempotentHint`,
  `openWorldHint`).
- The internal catalogue is restructured: each tool declares domain,
  profile, and an annotation preset — new tools cannot skip the decision.
- Tool descriptions were tightened for decision-making (shorter, fewer
  cross-tool warnings) without changing names or input schemas.

### task.due webhooks

A due-date watchdog tick (`TASKAGENT_DUE_TICK_SECS`, default 60 s, `0`
disables) now emits a `task.due` event when an active task's `due_at`
passes — once per (task, deadline) value, deduped across restarts via the
`task_due_notifications` projection (migration 0032). Webhook
subscriptions pick it up by kind like any other event; changing the
deadline re-arms the notification.

### Generic fenced leases (multi-agent coordination, P1)

`work_leases` generalizes from exclusive path globs to mode-aware
resource leases (migration 0033): `mode` (`exclusive` | `shared_read` |
`review` | `intent` — only writes conflict, intent is advisory),
`target_uri` with scheme-dispatched conflict matching (`file://` glob
overlap; `artifact://`/`contract://`/`env://` exact canonical match),
and a monotonic per-resource `fencing_token` issued inside the same
transaction as the grant — stale holders cannot pass
`check_fencing_token` after a re-grant. `reserve_files` (HTTP `/v1/leases`
and the MCP tool) accepts optional `targets` + `mode` and returns leases
carrying tokens; the legacy `paths`-only call is unchanged. The MCP
`taskagent_healthz` tool moved into the `default` profile.

### Auto-append into Interview / Human Log

Project activity now writes itself into the auto-created documents:
agent activity (agent task ops, runs, run notes) appends to
**Interview**, human milestones (user task ops, plan completion,
project renames) append to **Human Log**. Per-project toggles — ON by
default, also for pre-existing projects — live at
`GET/PATCH /v1/projects/{id}/settings` and the MCP tools
`taskagent_project_settings_get` / `_update`; changes are event-sourced
(`ProjectSettingsChanged`, migration 0034) so other clients update in
realtime. See docs/guides/documents-auto-append.md.

### Async AI operation events (WS Channel::AiOps)

Server-side AI operations (`/v1/ai/decompose/{task}`,
`/v1/ai/analyze-complexity/{plan}`) now push typed progress events —
`ai_operation_started` → `ai_operation_phase_changed` (`llm_call`,
`apply`) → `ai_operation_completed` (`ok` / `error: …`) — on the new
WS `AiOps` channel, so clients render progress without polling. The
HTTP responses are unchanged.

### Workspace auto-resolution

`POST /v1/workspace-registry/resolve` maps a filesystem root onto its
logical workspace + default project, creating and binding both on first
contact (longest project-root prefix wins; `create:false` probes only;
`workspace_id` targets an existing workspace). New MCP tools (full
profile): `taskagent_workspace_resolve` (persists the resolved project
as the scope default), `taskagent_workspace_list`, and
`taskagent_project_move_workspace`.

### Scheduler correctness (multi-agent coordination, P2)

The plan task resolver now honors cross-task `Blocks` relations (same
semantics as `can_start`) in every dispatch path — `drain_next`,
`ready_drain`, `next-task`, and `plan_progress.next_ready` — so
concurrent agents can no longer each grab one side of a mutually
blocking pair. Multi-target lease reservation is all-or-none over
canonically sorted targets inside a single immediate transaction,
making opposite-order bulk acquisition deadlock-free.

### AI prompt hardening

Grounding context (task bodies, comments, event payloads) interpolated
into AI prompts is now fenced as explicit untrusted data with embedded
fence-escape neutralization. See docs/guides/ai-agent.md.

## 0.2.0

### One unified `taskagent` binary

The stdio MCP server is now a subcommand of the CLI: a single `taskagent`
binary is the CLI, the launcher, and the MCP server — one artifact configures
and serves everything.

- `taskagent mcp` serves the stdio MCP server, superseding the standalone
  `taskagent-mcp` binary. Register it with
  `claude mcp add taskagent -- taskagent mcp`.
- Bare `taskagent` (no subcommand) prints HTTP-MCP connect instructions. With
  credentials it emits a ready-to-paste snippet for whatever server
  `~/.agents/taskagent/credentials.json` points at.
- `taskagent install --claude` writes the project policy (`CLAUDE.md`) and the
  `.omc` guard, now the single source of truth for that text (shared
  byte-for-byte with the `taskagent-claude` plugin).

### Cloud-agnostic core

- The `taskagent` binary references no hosted service: it reads a generic
  `server_url` + `token` from credentials and works against any server,
  self-hosted or otherwise.
- Removed the cloud-flavored `install.sh` and its GitHub Pages workflow from
  the OSS repository; cloud onboarding is no longer shipped from OSS.

### Server

- The binary download endpoint moved from
  `/v1/downloads/taskagent-mcp/{platform}` to
  `/v1/downloads/taskagent/{platform}` and serves the unified binary.

### Client plugins

- `taskagent-claude` delegates policy writing to the `taskagent` binary when it
  is on `PATH`, falling back to its bundled Node writer otherwise.
- `taskagent-cursor` no longer hardcodes a hosted URL in its install hints.

### Docs

- README rewritten as a leaner, overview-first document.

## 0.1.0

Initial OSS release preparation:

- Local-first task runtime with REST, WebSocket, and MCP surfaces.
- Event-sourced task, project, comment, agent inbox, webhook, and auth core.
- Claude, Cursor, and Codex companion packages for agent workflows.
- Public repository cleanup for self-hosted development.
