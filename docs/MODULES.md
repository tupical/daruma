# Module Registry

This document is the **canonical registry** of every TaskAgent module —
each app and crate, classified by *kind* (`core`, `transport`, `client`,
`embed`, `integration`), with a per-module manifest describing the
contract it relies on. Adding a new module = adding a row here and a
matching `module.toml` (or equivalent) in its source tree.

The runtime split is formalised in
[docs/MODULE_CONTRACT.md](MODULE_CONTRACT.md); the underlying invariants
(commands → events → projections, no backdoors around the bus) live in
[ARCHITECTURE.md](ARCHITECTURE.md).

## Kinds

| Kind          | Meaning                                                                                       |
|---------------|-----------------------------------------------------------------------------------------------|
| `core`        | Domain, command/event runtime, storage, auth. Stable contract; minor versions backwards-compat. |
| `transport`   | Speaks HTTP / WS / MCP / webhook to clients; owned by core (lives in `crates/` or `apps/`). |
| `client`      | Consumer of `/v1/*`; ships its own UI/CLI binary. May be replaced freely.                     |
| `embed`       | Runs `taskagent-core` in-process (no network). Desktop today; mobile later if size allows.    |
| `integration` | Speaks to a third-party system (GitHub, Slack, …). Planned, no shipped impls yet.             |

## Registry

| Module                 | Path                       | Kind          | Lang  | Status     | Owner          | Contract dep |
|------------------------|----------------------------|---------------|-------|------------|----------------|--------------|
| `taskagent-shared`     | `crates/shared/`           | `core`        | Rust  | shipped    | core           | —            |
| `taskagent-domain`     | `crates/domain/`           | `core`        | Rust  | shipped    | core           | shared       |
| `taskagent-events`     | `crates/events/`           | `core`        | Rust  | shipped    | core           | shared+domain |
| `taskagent-core`       | `crates/core/`             | `core`        | Rust  | shipped    | core           | events+storage |
| `taskagent-storage`    | `crates/storage/`          | `core`        | Rust  | shipped    | core           | events        |
| `taskagent-auth`       | `crates/auth/`             | `core`        | Rust  | shipped    | core           | shared        |
| `taskagent-api-dto`    | `crates/api-dto/`          | `core`        | Rust  | shipped    | core           | domain+events |
| `taskagent-server`     | `apps/server/`             | `transport`   | Rust  | shipped    | core           | core+auth     |
| `taskagent-sync`       | `crates/sync/`             | `transport`   | Rust  | shipped    | core           | events        |
| `taskagent-webhooks`   | `crates/webhooks/`         | `transport`   | Rust  | shipped    | core           | events        |
| `taskagent-mcp`        | `crates/mcp/`              | `transport`   | Rust  | shipped    | core           | server (HTTP) |
| `taskagent-ai`         | `crates/ai/`               | `transport`   | Rust  | shipped    | core           | domain+events |
| `taskagent-web`        | `../taskagent-web/` (repo) | `client`      | Rust/WASM | shipped | clients        | `/v1/*` + WS  |
| `taskagent-cli`        | `apps/cli/`                | `client`      | Rust  | shipped    | clients        | `/v1/*`       |
| `taskagent-desktop`    | `apps/desktop/`            | `embed`       | Rust (GPUI) | wip    | clients        | `taskagent-core` in-process |
| `taskagent-mobile`     | `apps/mobile/`             | `client`      | Rust+Tauri 2 | wip     | clients     | `/v1/*` (HTTP) |
| GitHub integration     | `integrations/github/`     | `integration` | — | planned    | integrations   | webhooks + `/v1/*` |
| Slack integration      | `integrations/slack/`      | `integration` | — | planned    | integrations   | webhooks + `/v1/*` |

Legacy: `apps/web/` (vanilla TS) and `apps/mcp-shim/` (Node shim) are
retired — see `git log` for the cut-over commits. They remain in-tree
only until their replacements ship a final feature parity check.

## Per-module manifest

Every non-trivial module SHOULD carry a `module.toml` (or `module.json`
for non-Rust modules) at its source root with the following shape:

```toml
[module]
name           = "taskagent-web"
kind           = "client"                    # core | transport | client | embed | integration
status         = "shipped"                   # planned | wip | shipped | retired
owner          = "clients"
contract       = "/v1/*"                     # which surface this module consumes
contract_min   = "v1.0"                      # minimum API version required

[capabilities]
needs = ["TaskRead", "TaskWrite", "PlanRead", "PlanWrite"]
optional = ["AdminTokens"]

[core]
source       = "git"
repo         = "https://github.com/tupical/daruma"
version      = "0.1.0"
tag          = "taskagent-v0.1.0"
dev_override = "vendor/oss -> ../taskagent"
```

The manifest is the source of truth for:

- **CI audit-grep** (W4.1) — verifies `kind != "core"` modules do not
  import `apps::*` directly, only public types from `crates/*` or
  `crates/core/src/embed.rs` (W2.1).
- **Capability declaration** — modules must list capabilities they
  actually call; the test harness can mint tokens with exactly that
  scope and fail if a route reaches for more.
- **Core dependency declaration** — standalone app repositories must
  record the immutable OSS git tag they consume; `vendor/oss` is only a
  local development override.
- **Docs generation** — the table in this file is intended to be
  generated from manifests once the toolchain is in place.

`module.toml` files are optional for already-shipped modules — they will
be backfilled as part of the W4 audit step.
