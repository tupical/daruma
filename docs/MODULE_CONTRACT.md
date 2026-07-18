# Module ⇄ Core Contract

This is the formal SLA between **Daruma core** (the `daruma-*`
crates plus `apps/server` and the `daruma mcp` stdio entry in `apps/cli`) and every **module** (client,
embed, integration) that consumes it. See
[docs/MODULES.md](MODULES.md) for the live registry of modules.

The goal is operational: a team building `apps/mobile/` or
`integrations/github/` must be able to ship without waiting on core,
and core must be able to evolve internals without breaking shipped
modules silently.

Releases and app dependency pinning are defined in
[docs/RELEASES.md](RELEASES.md). External app repositories should depend on an
immutable Daruma OSS git tag and use `vendor/oss` only as a local development
override.

## What core guarantees

1. **REST stability on `/v1/*`.** Any breaking change cuts a new prefix
   (`/v2/*`) and sets a `Sunset:` header on the deprecated routes at
   least **six months** before removal. Backwards-compatible additions
   (new fields, new endpoints) are minor and require no client work.
2. **Event-schema stability.** `crates/events/src/event.rs` is the
   canonical wire schema. New variants and new optional fields are
   additive; removing a variant requires a `/v2` API cut and a
   migration in `crates/storage/migrations/`.
3. **Idempotency contract** — every mutating command may carry a
   `client_command_id: UUIDv4`. Two dispatches with the same id produce
   the same `EventEnvelope.event_id` and the same `seq` (see
   `processed_command_ids` table). Modules SHOULD send one for every
   user-visible action.
4. **Capability semantics.** A token with `TaskRead` cannot mutate; a
   token with `Admin` is the wildcard. Capabilities never narrow
   silently — removing a capability from an endpoint requires a `/v2`
   cut and a Sunset notice.
5. **WS protocol stability.** Subscribe filters (`Channel::{Tasks,
   Comments, AgentStatus, Presence, Webhooks, Plans, Runs}`) and the
   `Resync` flow on `Lagged` are stable. `Hello` capabilities are
   additive — new fields appear, old fields persist until a `/v2` cut.
6. **`/healthz` versioning.** `/healthz` returns
   `{status, version, core_version, api_version}` so probes can detect
   drift without parsing build manifests (see §3.4 W2.2).

## What modules MUST NOT do

1. **No `apps::*` imports.** A module never imports symbols from
   another `apps/*` crate. The only legal cross-crate dependency for an
   embed module is the public surface of
   [`crates/core/src/embed.rs`](../crates/core/src/embed.rs) (W2.1) plus
   `daruma-domain` types.
2. **No direct DB access.** Modules go through the HTTP/WS/MCP API or
   the embed surface. SQL lives in `crates/storage/` only.
3. **No state mutation outside the bus.** All writes go through
   `CommandBus::dispatch`. WebSocket clients receive events from the
   bus; they do not write back to it.
4. **No private capability bits.** Modules use only capabilities
   declared in `daruma_auth::Capability`. New requirements go through
   a core-side PR that adds the bit and gates the route.

## Versioning

| Surface          | Stable contract            | Breaking-change ritual |
|------------------|----------------------------|------------------------|
| REST `/v1/*`     | Semver-minor additive      | New `/v2/*` + 6-month `Sunset:` header on `/v1/*` |
| WS `/v1/ws`      | Additive `Hello` fields    | New subproto `daruma.v2`; `daruma.v1` deprecated with a Sunset window |
| MCP tools        | Tool names + JSON schema   | New tool name suffix `_v2`; old tool returns `"deprecated": true` for one minor cycle |
| Webhooks         | Body = `EventEnvelope`     | Bumped `X-Daruma-Schema-Version` header; consumers pick |
| Event schema     | Additive variants/fields   | New variant requires migration + ROADMAP entry |

`api_version` in `/healthz` reflects the *minimum-promise* version the
running binary serves (today `"v1"`).

The Rust crate version and the git release tag identify the OSS core release.
Modules record the tag they consume in `module.toml [core]`.

## Capability declaration

A module declares the capabilities it relies on in its `module.toml`.
The CI gate uses this declaration to mint a test token with exactly
that scope and runs the module's smoke tests; if a route reaches for
something not declared, the test 403s and CI fails. This keeps the
"least privilege" invariant honest at the module boundary.

## Error contract

All HTTP error responses are JSON of the shape:

```json
{ "error": { "code": "<stable_snake_case_code>", "message": "<human>" } }
```

The `code` strings are stable identifiers consumers may switch on. The
inventory lives in `daruma_shared::CoreError::code()`:
`not_found | validation | conflict | storage_error | sync_error |
ai_unavailable | serialization_error | io_error | unauthorized |
forbidden`, plus route-specific codes (`auth_missing`, `auth_invalid`,
`task_blocked`, `cycle_detected`, `idempotent_replay`, …) that are
introduced via core PRs and documented in their respective sections of
[ARCHITECTURE.md](ARCHITECTURE.md).

## Embed mode (in-process core)

`apps/desktop` runs the core inside the same process — no HTTP, no
loopback socket. The only legal entry point is
`crates/core/src/embed.rs` (W2.1), which re-exports `{Db, EventBus,
CommandBus}` with the same semantics as the network path. Embed
clients receive `EventEnvelope`s via the in-process `EventBus`, same
schema, same `seq` ordering.

Embed-mode modules do not need a network capability check (no token);
they are trusted by virtue of running in the same address space. They
must still respect the command/event invariant — UI never mutates state
directly, only via `CommandBus::dispatch`.

## AI layer primitive/product boundary

`daruma-ai-infra` is a **primitive** crate: provider-neutral infrastructure
(HTTP client, config, prompt renderer, tool schemas, injection hardening) with
no knowledge of task operations. It lives in the OSS core and is consumed by
upper layers through `vendor/oss/crates/ai-infra`.

`daruma-ai` has been collapsed: consumers use `daruma-ai-infra` directly, and
the one remaining core AI operation (`analyze_complexity`) lives in
`apps/server/src/ai.rs` as a deprecated delegation-shim until the cloud
cutover to the planning layer (`yatagarasu`).

AI operations that are **product** concerns — parse, decompose, scope,
research — live in the upper-layer repos (`intake_oss`, `sensemaking_oss`,
`planning_oss`). They depend on `ai-infra` through `vendor/oss`; the
dependency arrow never reverses. Do not add parse/decompose/scope/research back
to `daruma-ai-infra` or the server shim.

## Pipeline position (MeiSei)

Daruma is the terminal execution layer of the MeiSei maturity pipeline
(`torii → satori → enma → yatagarasu → fujin → daruma`) — see
[ADR: terminal-execution-layer](adr/terminal-execution-layer.md). The
module/core contract above governs consumers of the Daruma API; it does
not, and must not, grow a symmetric contract in the other direction — no
module or core PR may add a `Command` that creates work for another
pipeline layer. This mirrors the one-way dependency arrow already
established for the AI layer above (product-layer repos depend on
`daruma-ai-infra`, never the reverse). `hyakki` is
out-of-band (clustering/observability), not a pipeline hop that consumes
daruma's output.

## Lifecycle

- **Planned** — listed in [docs/MODULES.md](MODULES.md), no source tree
  yet. Anyone may claim ownership by opening a `/plan` and proposing a
  scaffold PR.
- **WIP** — scaffold merged; module exists but is not advertised. No
  contract guarantees the other direction.
- **Shipped** — appears in the registry as `shipped`; core promises the
  stability terms above; module promises to use only the documented
  surfaces.
- **Retired** — module is being removed; entry remains in the registry
  with `status = retired` until the cut-over commit is final, then
  drops out at the next docs cleanup.
