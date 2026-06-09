# Changelog

This file tracks public, user-visible changes for TaskAgent releases.

For the current pre-release history in Russian, see [CHANGELOG.ru.md](CHANGELOG.ru.md).

## Unreleased

### MCP tool-surface profiles

`tools/list` is now profile-gated: the new `default` profile advertises a
compact, workflow-first surface of 30 tools; `full` keeps the complete
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
