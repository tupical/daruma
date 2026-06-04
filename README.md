# TaskAgent

[![License: Apache-2.0 WITH Commons-Clause](https://img.shields.io/badge/license-Apache--2.0%20WITH%20Commons--Clause-blue.svg)](LICENSE)

**Your last next task manager.**

Crafted for speed and collaboration with humans and AI. TaskAgent is an
agent-native, local-first task runtime: a task manager for agents, and an
agent for agents.

| | |
|---|---|
| This repo | [`tupical/taskagent`](https://github.com/tupical/taskagent) |

This is **not** a Jira/Notion/ClickUp clone. AI agents are first-class
users; tasks are events in a realtime system; the desktop app runs fully
offline. Keyboard-first UX, command-palette driven.

## Stack

| Layer    | Tech                                                                |
| -------- | ------------------------------------------------------------------- |
| Desktop  | Rust + [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) |
| Server   | Rust + Axum + Tokio                                                 |
| Storage  | SQLite via SQLx (local-first, append-only event log + projections)  |
| Sync     | WebSocket-bridged `EventEnvelope` stream + per-agent inbox cursor   |
| Auth     | Bearer tokens (argon2id) with capability bit-flags + project scope |
| Webhooks | HMAC-SHA256 signed outbound POST per match                          |
| MCP      | JSON-RPC 2.0 over stdio — 16 tools for Claude Desktop / Inspector   |
| AI       | OpenAI Responses API, tool-calling only — emits commands, never writes DB |
| Web      | Rust + [Leptos](https://leptos.dev) 0.7 CSR → WASM (Trunk) — standalone [`taskagent-web`](../taskagent-web) repo, talks to `/v1/*` + `/v1/ws` |

## Client plugins

TaskAgent includes local client glue for common agent environments.

| npm package | Role | Repository / path |
| --- | --- | --- |
| [`taskagent-cursor`](clients/cursor-plugin/) | Cursor MCP registration (`~/.cursor/mcp.json`), deeplink install, rules/commands, OMC guard | `clients/cursor-plugin` |
| [`taskagent-claude`](clients/claude-plugin/) | Claude Code + oh-my-claudecode orchestration (`start`, `doctor`, `setup`) | `clients/claude-plugin` |

Client packages read credentials from `~/.agents/taskagent/credentials.json` and register the server either as remote HTTP MCP (`/v1/mcp`, preferred for Cursor) or stdio `taskagent-mcp` fallback. Agent data directory: `~/.agents/taskagent/` — see [docs/guides/mcp-client.md](docs/guides/mcp-client.md).

## Layout

```
apps/
  desktop/      GPUI client (offline-capable, embeds the local engine)
  server/       Axum HTTP + WS server (auth, /v1, webhooks, inbox)
  mcp/          stdio MCP fallback (taskagent-mcp) — HTTP-hop to apps/server
crates/
  shared/    IDs (TaskId, ProjectId, EventId, AgentId, CommentId,
             TokenId, WebhookId, WebhookDeliveryId), time, error types
  domain/    Task (+ started_at/completed_at), Project, Comment, Actor,
             AgentAction, Status, Priority
  events/    Event enum (mechanical + semantic + comment), EventEnvelope,
             EventStore trait, EventBus, Channel enum
  storage/   SQLite implementation of EventStore + projection repos
             (TaskRepo, ProjectRepo, CommentRepo, TokenRepo,
              AgentInboxRepo, WebhookRepo)
  core/      Commands, CommandHandler (emits semantic events alongside
             mechanical ones), CommandBus
  sync/      WS v2 wire protocol (Hello, Subscribe filters, Snapshot,
             Resync on Lagged, Ping/Pong heartbeat) + Hub bridge
  auth/      Capability bit-flags, TokenScope, ApiToken, TokenStore
             trait, verify_bearer
  webhooks/  Webhook model, HMAC signer, WebhookStore trait, dispatcher
  mcp/       JSON-RPC 2.0 server, 16 tool definitions, ApiClient
  ai/        OpenAI Responses-API client + tool schemas + parsers
```

## Build

This workspace is set up for native Rust development on the host. Docker is
kept only for optional release/runtime parity; it is not used for everyday
`cargo` work.

```sh
cargo build --workspace
cargo run -p taskagent-desktop          # desktop client (offline-first)
cargo run -p taskagent-server           # auth + WS + webhooks server
cargo run -p taskagent-mcp-bin          # stdio MCP server
```

Common shortcuts live in `Justfile`:

```sh
just check        # cargo check --workspace
just test         # cargo test --workspace
just clippy       # cargo clippy --workspace --all-targets -- -D warnings
just server       # API on :8080; data in ~/.agents/taskagent/data
```

The desktop app does **not** require the server. The server is needed
for cross-device sync, the web companion, agent realtime, webhooks, and
the MCP server.

## Web frontend

The browser UI lives in the standalone **[`taskagent-web`](../taskagent-web)**
repo (Leptos CSR → WASM). In OSS it is a read-only observability viewer for
tasks, plans, documents, and realtime agent activity. This server is a bare API
+ MCP backend and no longer bundles or serves static web assets; deploy the UI
separately and point it at this server's `/v1/*` + `/v1/ws`.

```sh
# In a sibling checkout next to this repo:
git clone <taskagent-web> ../taskagent-web
cd ../taskagent-web && sh scripts/link-oss.sh   # vendor/oss → ../taskagent
trunk serve            # dev: proxies /v1/* to http://127.0.0.1:8080
trunk build --release  # prod bundle into dist/
```

Standalone apps should pin TaskAgent OSS by git tag for releases; `vendor/oss`
is a local development override. See [`docs/RELEASES.md`](docs/RELEASES.md).

## Quick start (server side)

```sh
# 1. Boot the server. On the first run it generates a long-lived `svc`
#    admin token and prints it once to stderr; the same string is also
#    written to <data_dir>/bootstrap.token (mode 0600 on Unix).
cargo run -p taskagent-server   # data: ~/.agents/taskagent/data

# 2. Save the token in an env var.
export TASKAGENT_TOKEN=ta_svc_…

# 3. Drive the API.
curl -H "Authorization: Bearer $TASKAGENT_TOKEN" http://localhost:8080/v1/tasks
```

`TASKAGENT_DATA_DIR` defaults to `~/.agents/taskagent/data` for native runs. Docker runtime
sets it explicitly to `/app/data`.

## How an agent listens

A subscribed agent always receives realtime semantic signals — never raw
polling. Two transports cover the same `EventBus`:

### Long-poll inbox (HTTP)

For agents that cannot hold a socket open, use the agent inbox cursor:

```sh
# Block up to 30 s for any event past the cursor; returns immediately if
# events have accumulated. Cap `long_poll` is 60 s.
curl -H "Authorization: Bearer $TOKEN" \
  "http://localhost:8080/v1/agents/$AGENT_ID/inbox?long_poll=30&max=100"

# Advance the cursor — idempotent, monotonic.
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"up_to_seq": 42}' \
  "http://localhost:8080/v1/agents/$AGENT_ID/inbox/ack"
```

### WebSocket subscription

For interactive agents (and the desktop), open `/v1/ws?token=…` and
subscribe with optional `since_seq` for catch-up plus `projects` /
`channels` filters:

```json
{"type":"subscribe","since_seq":0,"projects":["<uuid>"],"channels":["tasks","comments"]}
```

The server first sends `Hello { server_seq, capabilities }`, then a
`Snapshot` for any `since_seq` history, then live `Event` frames. When
the broadcast queue overflows the server emits a `Resync { from_seq,
dropped }` and the client re-subscribes with `since_seq = from_seq`.

### MCP (`tools/call`)

Cursor can connect directly to the server's remote MCP endpoint:

```sh
taskagent install --print-config cursor
```

The printed `mcp.json` snippet points at `/v1/mcp` and includes bearer
headers when `TASKAGENT_TOKEN` or `~/.agents/taskagent/credentials.json`
is present. Claude Desktop / the MCP Inspector can still spawn
`taskagent-mcp` over stdio and call the same tools:

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call",
 "params":{"name":"taskagent_inbox_pull",
           "arguments":{"agent_id":"<uuid>","long_poll_secs":30}}}
```

Plan hierarchy (§3.1 §3.3 W1–W3): create root plan, then reparent sub-plans:

```json
{"method":"tools/call","params":{"name":"taskagent_plan_create",
  "arguments":{"project_id":"prj_..","title":"Root","parent_plan_id":null}}}
{"method":"tools/call","params":{"name":"taskagent_plan_update",
  "arguments":{"id":"pln_..","parent_plan_id":"pln_root_id"}}}
{"method":"tools/call","params":{"name":"taskagent_plan_update",
  "arguments":{"id":"pln_..","parent_plan_id":null}}}
```

44 tools are advertised; the four required by the spec — `subscribe_project`,
`inbox_pull`, `comment`, `reopen` — are wired against the same HTTP
endpoints as `curl`, with bearer auth from `TASKAGENT_TOKEN`.

### Outbound webhooks

Register a URL once; every matching event gets POSTed within ~1 s with a
`X-Taskagent-Signature: hex(hmac_sha256(secret, body))` header.

```sh
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"url":"https://hook.example/incoming",
       "secret":"replace-me",
       "events":["task_reopened","task_commented"]}' \
  http://localhost:8080/v1/webhooks
```

## Design

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — crate contracts, URL layout,
capability gating, command → event → projection → fanout.

See [`docs/guides/ai-agent.md`](docs/guides/ai-agent.md) and
[`CONTRIBUTING.md`](CONTRIBUTING.md#code-style) for engineering constraints.

Contributing, changelog, and code of conduct: [`CONTRIBUTING.md`](CONTRIBUTING.md),
[`CHANGELOG.md`](CHANGELOG.md), [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).
