# ADR: Daruma as the Terminal Execution Layer of the MeiSei Pipeline

## Status

Accepted. Decided 2026-07-17 (owner decision).

## Context

Daruma is one tool inside the wider **MeiSei** maturity pipeline:

```
torii → satori → enma → yatagarasu → fujin → daruma
intake   sensing  decisions planning  actions execution
```

Each layer accepts only the persisted output of the previous layer
(enforced server-side by the platform's pipeline orchestrator, not by
prompt discipline); `daruma` receives a mature Action Packet through a
handoff and turns it into task/plan state.

A prior task ("Терминальность daruma: зафиксировать отсутствие
downstream-выхода") asked to document that daruma is the pipeline's last
layer — it has no output to a next hop. That work was blocked: this
repository's own `README.md` pipeline diagram listed a layer *after*
daruma, `hyakki` ("clusterer"), which contradicted the terminality claim
and had no ADR resolving the conflict.

The owner has since decided: **`hyakki` is out-of-band — a clustering /
observability tool, not the pipeline's next maturity hop.** It does not
consume daruma's output as pipeline input and is not part of the
`torii → … → daruma` hop chain. This ADR records that decision and the
terminality invariant it unblocks.

### Code audit (basis for this decision)

Before this ADR, the `Command` enum (`crates/api-dto/src/command.rs`,
~45 variants) and `crates/core/src/handler.rs` were audited for any
primitive that creates work in another layer. Findings:

- **No command creates work in another layer/system.** Every variant
  operates on daruma-native entities only: `Task`, `Plan`, `WorkUnit`,
  `Document`, `Run`, `AgentSession`, `Comment`, `Claim`, `WorkLease`,
  `HandoffContract`, `Rule`, `Evidence`, `Artifact`.
- **`RequestHandoff` / `AcceptHandoff` / `RejectHandoff`** operate on
  `WorkUnitId → WorkUnitId` (`crates/domain/src/handoff.rs`) — intra-
  execution-layer agent↔agent handoffs, not a cross-layer exit.
- **`ExternalRef`** (`crates/domain/src/external_ref.rs`) is an
  *inbound* identity-correlation map (`external_id → internal_id`) used
  for idempotent intake from upstream systems — not an outbound
  "create work elsewhere" primitive.
- **Webhooks** (`crates/webhooks`) are generic, user-configured outbound
  HTTP push subscriptions for observability/integration — a
  notification/read-back mechanism, not a way to create work in another
  pipeline layer.
- **Zero references** to `fujin`/`hyakki`/pipeline-advance calls as
  *outbound* calls anywhere in `crates/`. The pipeline direction that
  does exist is inbound: `daruma_plan_materialize`'s payload shape
  mirrors the upstream `fujin::NewPlanWithTasks` (fujin hands off to
  daruma, not the other way around).

## Decision

Daruma is the **terminal, execution-only layer** of the MeiSei maturity
pipeline. There is no downstream-layer output primitive:

1. Daruma materializes tasks/plans only from a mature Action Packet
   handed off by `fujin` (plan-only intake, see the plan-materialize
   invariant); it never emits work back upstream or forward to a
   further maturity hop.
2. Results leave daruma only as **read-back** — observability, evidence,
   artifacts, activity/event log, webhook notifications — never as a
   command that creates work for another layer.
3. Intra-execution **agent → agent handoffs** (`RequestHandoff` /
   `AcceptHandoff` / `RejectHandoff` between `WorkUnit`s) remain. They
   are within-layer coordination, not a cross-layer exit, and this ADR
   does not restrict them.
4. **`hyakki` is out-of-band.** It is a clustering/observability tool
   that may read daruma's data (same as any other read-back consumer,
   e.g. a webhook subscriber), but it is not a pipeline hop: daruma does
   not hand off to it, and it is not part of the
   `torii → satori → enma → yatagarasu → fujin → daruma` maturity route.

## Non-Goals

- This ADR does not forbid daruma from emitting observability data
  (webhooks, events, metrics) that external tools — including `hyakki`
  — may passively consume. Terminality is about *pipeline hops*, not
  about who is allowed to read daruma's output.
- This ADR does not freeze the `Command` enum forever. Future OSS PRs
  may add primitives; if any such primitive were to create work in
  another maturity layer, that would revoke this invariant and requires
  a new ADR, not a silent addition.

## Consequences

- `docs/ARCHITECTURE.md` and `docs/MODULE_CONTRACT.md` state the
  terminality invariant and cite this ADR.
- `README.md`'s pipeline diagram no longer lists `hyakki` as a hop after
  daruma; it is called out as out-of-band in a separate line.
- Any future proposal to give daruma a downstream-layer output (e.g.
  daruma opening work in `hyakki` or any other layer) must supersede
  this ADR explicitly, not bypass it via an unreviewed feature.
