# ADR: WorkUnit + Artifact Ownership — vocabulary and core decisions

## Status

Accepted as the P0 spec for the "WorkUnit + Artifact Ownership coordination
layer" plan. Locks terminology and the load-bearing decisions before any
code; later phases (P1–P7) implement against this document.

## Context

Daruma already coordinates parallel agents at two levels: atomic task
claims (`agent_claims` CAS, `plan_drain_next` / `ready_drain`) and exclusive
path-glob work leases with auto-release (see
[parallel-agent-isolation.md](parallel-agent-isolation.md)). Both shipped.
What does not exist yet:

- a dispatchable unit *smaller than a task*, so several agents can safely
  work one large task;
- an ownership/lifecycle catalog for the *results* of work (contracts,
  schemas, docs, file groups) — today ownership lives in comments;
- fencing: a stale lease holder can still commit a write after losing its
  lease (no token validation anywhere);
- handoff as a first-class gate (knowledge transfer is buried in comments).

This layer moves contention from task-level to artifact/resource-level.

## Vocabulary (locked)

| Term | Definition |
|------|------------|
| **Stage** | A phase of a larger effort. **Decision: Stage = `Plan` with `parent_plan_id`** — `plans.parent_plan_id` and the `ParentPlan` WorkspaceGraph edge already exist; there is **no new Stage table**. |
| **WorkUnit** | The minimal dispatchable unit of work, subordinate to a task (`task_id`), optionally attached to a stage plan (`stage_plan_id`). Statuses: `todo → ready → in_progress → blocked → review → done / cancelled`. |
| **Artifact** | A named, versioned result of work (API contract, DB schema, doc, test suite, UI component, file group) registered in the Artifact Registry with its own lifecycle: `proposed → draft → reviewed → approved → implemented → verified → deprecated`. |
| **Handoff** | A first-class contract between two work units: required artifacts + required artifact state + acceptance checklist. Gates dispatch of the receiving unit. |
| **Lease (mode)** | A TTL'd reservation on a resource URI. Modes: `exclusive` (one holder), `shared_read` (many readers coexist), `review` (conflicts with writes), `intent` (advisory, never hard-blocks). |
| **Fencing token** | A per-resource monotonic counter issued at lease acquisition. Writes carry the token; a stale holder's write with an outdated token is rejected. |

## Accountability roles (locked — do NOT merge these fields)

Four distinct roles, stored as separate fields; collapsing any pair loses
information the scheduler or audit needs:

| Role | Meaning | Lifetime |
|------|---------|----------|
| **owner** | Responsible for the outcome (task/work-unit/artifact level). | Survives claims; reassigned explicitly. |
| **holder** | Currently doing the work (claim/lease holder). | Transient — TTL'd, auto-released. |
| **steward / reviewer** | Approves lifecycle transitions (e.g. artifact `reviewed → approved`). | Per artifact kind / scope. |
| **scheduler** | The actor that dispatched the unit (drain call, plan fanout, human assignment). | Recorded per dispatch for audit/mining. |

## Artifact URI scheme

```
artifact://<kind>/<name>        # registry entries: artifact://api/users
file://<repo-relative-path>     # path/glob resources (existing path leases)
contract://<name>[@<version>]   # interface contracts: contract://api/dashboard@v1
env://<name>                    # shared environments/services: env://staging-db
```

Canonicalization rules (prevent URI drift):

- scheme and authority are lowercase ASCII; `name` segments are
  `kebab-case`, `/`-separated, no trailing slash, no `.`/`..` segments;
- `file://` URIs are repo-relative, `/`-separated, globs allowed (`*`,
  `**`) — same normalization as existing path leases;
- optional `@<version>` suffix only on `contract://`; everywhere else
  version is a registry field, not part of the identity;
- aliases resolve at write time: the registry stores the canonical URI and
  rejects registration of a new URI that canonicalizes onto an existing
  one (case/separator variants cannot create parallel identities).

Conflict matching dispatches on scheme: `file://` → path-glob overlap
(existing `paths_overlap`); `artifact://`, `contract://`, `env://` → exact
match on the canonical URI.

## Lazy activation (locked)

The work-unit/artifact layer is invisible for simple work. It activates for
a task/plan **iff** any of:

1. AI complexity for the task is `high` (`task_complexity_hints`);
2. `active_agents_on_plan >= 3`;
3. the user explicitly enables multi-agent mode for the plan/project;
4. the task references `>= 3` registered artifacts;
5. repeated contention is observed (`task_contested` or file-conflict
   events on the same task/plan, `>= 3` within a rolling 24 h window).

Otherwise simple tasks keep today's `task claim` path untouched. **The
guardrail is the simple-task median lead time: if it regresses, lazy
activation is broken and must be fixed before scaling work continues.**

## Event taxonomy (to be implemented by later phases)

Payloads are designed to be mineable later (XES-friendly: every event
carries actor, timestamps, and case ids — task/work-unit/artifact):

- **WorkUnit:** `WorkUnitCreated`, `WorkUnitClaimed`, `WorkUnitStarted`,
  `WorkUnitBlocked { reason }`, `WorkUnitCompleted { produced_artifacts,
  outcome, next_suggested_units }`, `WorkUnitReleased`.
- **Artifact:** `ArtifactRegistered`, `ArtifactOwnerAssigned`,
  `ArtifactStatusChanged { from, to, by }`, `ArtifactChanged`,
  `ArtifactWriteCommitted { fencing_token }`, `ArtifactDeprecated`.
- **Lease:** `ArtifactLeaseAcquired { mode, target_uri, fencing_token }`,
  `ArtifactLeaseReleased`, `ArtifactLeaseRejected { holder, mode }`,
  `ArtifactLeaseExpired`.
- **Handoff:** `HandoffRequested { from_wu, to_wu, artifacts,
  acceptance_criteria }`, `HandoffAccepted { handoff_id, by, notes }`,
  `HandoffRejected { handoff_id, reason, required_changes }`.
- **Responsibility (advisory):** `ResponsibilityPatternSuggested`,
  `ResponsibilityPatternAccepted`, `ResponsibilityPatternRejected`.

## Phase map (implementation order = risk/leverage)

| Phase | Scope | Builds on |
|-------|-------|-----------|
| P1 | Generic fenced leases: `mode` + `target_uri` + `fencing_token` on `work_leases` | shipped path leases (migration 0031) |
| P2 | Scheduler correctness: Blocks-aware `NextTaskResolver`, canonical-ordered all-or-none bulk acquire | P0, P1 |
| P3 | WorkUnit entity + `work_unit_drain_next` | P1, P2 |
| P4 | Artifact Registry on WorkspaceGraph (+ impact() edge kinds) | P1, P3 |
| P5 | Handoff contracts as dispatch gates | P3, P4 |
| P6 | Inferred responsibility — advisory only, human accept required | P3 |
| P7 | Scale mode — gated by load tests at 10/50/100 agents | P3, P4, P6 |

## Non-goals

- No new Stage table (reuse plan hierarchy).
- No hard binding of work to "the right agent": capability fit is a
  scheduling *preference* with grace-period fallback; user override always
  wins.
- No generic workflow engine; gates are limited to dependencies, leases,
  and handoffs.
- iOS/Android replicas and pure P2P coordination are out of scope (see
  device-sync plan).
