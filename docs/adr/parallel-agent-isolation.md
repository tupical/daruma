# ADR: Parallel-Agent Isolation Model — File Leases over Worktree Ownership

## Status

Accepted for the parallel-agent coordination work (claim CAS + work leases).

## Context

Multiple agents are given one command — "close the tasks for this project" — and
must run in parallel without (a) taking the same task or (b) editing the same
files. Two layers solve this:

1. **Atomic task claiming** — `taskagent_plan_drain_next` resolves the next
   ready task and acquires an exclusive claim via a single-statement
   compare-and-set (`agent_claims`), with a claim-aware resolver + retry so
   concurrent callers each get a distinct task. (Implemented.)
2. **File-level coordination** — preventing two agents from editing the same
   files even across *different* tasks.

For layer 2 the field splits into two patterns:

- **Worktree-per-agent** (Aperant, Conductor, Crystal/Nimbalyst): each agent
  gets its own `git worktree` + branch; isolation is physical, conflicts are
  resolved at merge time behind a human review gate.
- **File leases** (this project): a lightweight, TTL'd registry of the path
  globs each agent is touching; overlap is rejected at reservation time. "Like
  git, but lighter" — directory/file granularity, no diff, auto-released on task
  close or TTL.

This ADR records which model TaskAgent adopts and why.

## Decision

**TaskAgent owns the file-lease model (B) and treats worktree isolation (A) as a
client-side deployment choice it records but does not manage.**

Concretely:

- `work_leases` (migration `0031`) + `taskagent_reserve_files` /
  `taskagent_release_files` / `taskagent_active_work` are the canonical
  coordination surface. Reservation is atomic (`BEGIN IMMEDIATE` + glob-overlap
  check); leases auto-release on `TaskClosed` and via the TTL sweeper.
- TaskAgent does **not** create, switch, or merge git worktrees. It is a tracker
  and coordination server, not a VCS driver. Orchestrators (OMC, Conductor-style
  runners) remain free to put each agent in its own worktree; when they do, the
  file leases simply become advisory within that agent's private tree and stay
  authoritative for any agents that *share* a working tree.

### Rationale

- **TaskAgent is provider/VCS-agnostic.** Owning worktree lifecycles would couple
  the server to git layout, branch policy, and merge tooling — exactly the
  coupling the event-sourced core avoids elsewhere (cf. WorkspaceGraph sidecar).
- **Leases work in both worlds.** Shared-tree agents need them; separate-worktree
  agents are unharmed by them. Worktree ownership, by contrast, is useless to a
  shared-tree deployment and forces a heavyweight merge step.
- **Cheap hot path.** A lease reserve/refresh is one local SQLite transaction —
  no clone/push/pull rate limits (the bottleneck Relace calls out for many
  concurrent agents). Coordination stays out-of-band from git.
- **Self-healing.** TTL + close-triggered release mean a crashed agent's leases
  evaporate; no orphaned worktrees to garbage-collect.

### Recorded, not managed: worktree binding

To support worktree-based orchestrators without owning the lifecycle, a future
increment may add optional `worktree_path` / `branch` metadata on the claim or
agent session and surface it in `taskagent_active_work`. This is descriptive
("agent X is in branch Y") — TaskAgent still never runs git itself.

## Consequences

- Parallel agents sharing a checkout are fully protected (claim CAS + leases).
- Orchestrators that prefer hard physical isolation layer worktrees on top; the
  lease layer degrades to advisory there, which is acceptable.
- If a future requirement demands server-driven worktree provisioning, it would
  be a separate component (an orchestration runner), not a change to the tracker
  core — keeping this ADR intact.

## Alternatives considered

- **Worktree ownership in the server (A).** Rejected: couples the tracker to git,
  duplicates what orchestrators already do, and fails shared-tree deployments.
- **Advisory-only registry.** Rejected for the default: the user requires hard
  conflict avoidance, so overlap blocks by default. (A read-only view of leases
  is still exposed via `taskagent_active_work`.)


## Addendum (P2): Blocks-aware drain + deadlock-safe bulk acquire

Two correctness fixes landed after the original ADR:

1. **`NextTaskResolver` honors cross-task `Blocks` relations.** The
   resolver previously consulted only `plan_tasks.depends_on`, so two
   agents could each claim one side of a mutually-blocking pair. It now
   skips candidates with a live blocker (same semantics as `can_start`),
   in every dispatch path (`drain_next`, `ready_drain`, `next-task`,
   `plan_progress.next_ready`).
2. **Bulk lease acquisition is deadlock-free by construction.** A
   multi-target reserve canonicalizes and sorts its targets, scans for
   conflicts, and grants all-or-none inside one `BEGIN IMMEDIATE`
   transaction — concurrent acquirers serialize instead of interleaving,
   so opposite-order acquisition cannot deadlock and a loser never keeps
   partial grants. Policy for agents: do not block while holding —
   release leases before waiting on human approval.
