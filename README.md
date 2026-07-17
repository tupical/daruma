# Daruma 達磨 — execution layer of Meisei

> **Meisei** 明晰 (“clarity”) is an open pipeline that carries raw intent through
> understanding → decision → plan → action to a finished result. **Daruma** is its
> **execution** layer — the runtime that takes a plan and drives it to *done*.

[![Meisei](https://img.shields.io/badge/meisei-明晰-1f2937.svg)](https://meisei.ru)
[![License: Apache-2.0 WITH Commons-Clause](https://img.shields.io/badge/license-Apache--2.0%20WITH%20Commons--Clause-blue.svg)](LICENSE)

Daruma is **not a task manager** — no Jira/Notion/ClickUp clone with an AI bolt-on.
It is an **agent-native, local-first execution runtime**: AI agents are first-class
users, every change is a realtime event, and the desktop app runs **fully offline**.
Keyboard-first, command-palette driven, and fast.

One binary — `daruma` — is the CLI, the launcher, and the MCP server your agents
talk to. Point Claude, Cursor, or Codex at it and they share the same plans, work
items, and live activity stream as you do.

> The binary and crate ids keep the `daruma` name for now; the project, repo, and
> brand are **Daruma** under the Meisei umbrella. A full id migration is tracked separately.

<sub>
torii · satori · enma · yatagarasu · fujin · <b>daruma</b>
&nbsp;—&nbsp; intake · sensemaking · decisions · planning · actions · <b>execution (terminal)</b>
</sub>

<sub>
hyakki is out-of-band (clustering/observability) — not a pipeline hop; daruma is the terminal layer, see <a href="docs/adr/terminal-execution-layer.md">ADR</a>
</sub>

## Why Daruma

- **🤖 Agent-native.** Agents subscribe to semantic signals and act — they
  never poll. Tasks, plans, and decomposition are MCP-first, not an afterthought.
- **💾 Local-first & offline.** An append-only event log with SQLite
  projections. The desktop app needs no server; sync is optional.
- **⚡ Realtime by design.** Every command becomes an event, fanned out over
  WebSocket and a per-agent inbox cursor with catch-up and resync.
- **🔌 One binary, any client.** `daruma` speaks MCP over stdio **and** HTTP
  (`/v1/mcp`). Cloud-agnostic: it talks to whatever server your credentials point at.
- **🔐 Capability-scoped.** Bearer tokens (argon2id) with capability bit-flags
  and project scope; HMAC-signed outbound webhooks.

## Stack

| Layer    | Tech                                                                |
| -------- | ------------------------------------------------------------------- |
| Desktop  | Rust + [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) |
| Server   | Rust + Axum + Tokio                                                 |
| Storage  | SQLite via SQLx (local-first, append-only event log + projections)  |
| Sync     | WebSocket-bridged `EventEnvelope` stream + per-agent inbox cursor   |
| Auth     | Bearer tokens (argon2id) with capability bit-flags + project scope  |
| Webhooks | HMAC-SHA256 signed outbound POST per match                          |
| MCP      | JSON-RPC 2.0 over stdio **and** HTTP (`/v1/mcp`) — one `daruma` binary |
| AI       | OpenAI Responses API, tool-calling only — emits commands, never writes DB |
| Web      | Rust + [Leptos](https://leptos.dev) 0.7 CSR → WASM — standalone [`daruma-web`](../daruma-web) repo, talks to `/v1/*` + `/v1/ws` |

## Quick start

Native Rust on the host — Docker is kept only for optional release/runtime parity.

```sh
# 1. Boot the server. On first run it prints a long-lived `ta_svc_…` admin
#    token once (also written to <data_dir>/bootstrap.token, mode 0600).
cargo run -p daruma-server          # API on :8080, data in ~/.agents/daruma/data

# 2. Drive the API.
export DARUMA_TOKEN=ta_svc_…
curl -H "Authorization: Bearer $DARUMA_TOKEN" http://localhost:8080/v1/tasks

# 3. Build the unified launcher and let it walk you through connecting.
cargo build -p daruma-cli           # produces the `daruma` binary
daruma                              # prints connect instructions for your setup
```

Handy shortcuts live in the `Justfile`: `just check`, `just test`, `just clippy`,
`just server`. The desktop app (`cargo run -p daruma-desktop`) runs offline and
needs no server — the server adds cross-device sync, the web companion, agent
realtime, webhooks, and remote MCP.

## MCP

`daruma` **is** the MCP server. The same tool surface — tasks, plans,
documents, sessions, runs, webhooks — is served over stdio and over HTTP.

```sh
# Claude Code / Claude Desktop (stdio)
claude mcp add daruma -- daruma mcp

# Cursor (remote HTTP MCP at /v1/mcp) — prints a ready-to-paste mcp.json
daruma install --print-config cursor
```

Auth comes from `$DARUMA_TOKEN` or `~/.agents/daruma/credentials.json`.
Call any tool the same way over either transport:

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call",
 "params":{"name":"daruma_inbox_pull",
           "arguments":{"agent_id":"<uuid>","long_poll_secs":30}}}
```

Agents receive realtime semantic signals (never raw polling) over a WebSocket
subscription (`/v1/ws`) or the HTTP long-poll inbox cursor — see
[docs/guides/mcp-client.md](docs/guides/mcp-client.md) and
[docs/guides/ai-agent.md](docs/guides/ai-agent.md).

### Client plugins

Optional local glue for popular agent environments, on top of the binary:

| npm package | Role |
| --- | --- |
| [`daruma-cursor`](clients/cursor-plugin/) | Cursor MCP registration, deeplink install, rules/commands |
| [`daruma-claude`](clients/claude-plugin/) | Claude Code + oh-my-claudecode orchestration (`start`, `doctor`, `setup`) |

## Docs

- **Architecture** — crate contracts, URL layout, capability gating, command →
  event → projection → fanout: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
- **Contributing / code style:** [`CONTRIBUTING.md`](CONTRIBUTING.md)
- **Changelog · Code of conduct:** [`CHANGELOG.md`](CHANGELOG.md) ·
  [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md)
