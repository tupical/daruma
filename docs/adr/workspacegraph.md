# ADR: WorkspaceGraph Sidecar Index

## Status

Accepted for P0.

## Context

TaskAgent already has canonical event storage plus read projections for tasks,
plans, documents, comments, and task relations. Upcoming WorkspaceGraph work
needs a fast local index for context queries such as "what is related to this
task?", "what changes if this task moves?", and "find relevant workspace items".

The graph should improve retrieval and impact analysis without making the core
task tracker harder to reason about. The existing event log remains the source
of truth; WorkspaceGraph is a derived read model.

## Decision

WorkspaceGraph will use a sidecar SQLite database named
`workspacegraph.sqlite`, stored next to the workspace's `taskagent.sqlite`.

The sidecar is rebuilt from the event log and maintained incrementally by
applying events after the canonical projections have accepted them. If the
sidecar is missing, corrupt, or behind, TaskAgent may reindex it without
changing canonical data.

We choose a sidecar over inline tables in `taskagent.sqlite` because:

- graph schema can evolve independently from task storage migrations;
- failed or partial indexing cannot corrupt canonical task data;
- users can delete and rebuild the graph index as cache-like state;
- future lexical/vector search tables can be added without bloating core
  projections;
- Hosted and local deployments can tune sidecar storage independently.

Inline tables in the primary database remain acceptable only for canonical
entities and relations that are part of TaskAgent's mutation model.

## Node Taxonomy

Initial node kinds:

| Kind | Stable id | Source |
|------|-----------|--------|
| `Project` | `project:<project_id>` | project projection |
| `Plan` | `plan:<plan_id>` | plan projection |
| `Task` | `task:<task_id>` | task projection |
| `Document` | `document:<document_id>` | document projection |
| `Comment` | `comment:<comment_id>` | comment projection |

Each node stores:

- `id`: graph id with kind prefix;
- `kind`: one of the node kinds above;
- `source_id`: canonical TaskAgent id;
- `project_id`: owning project when known;
- `title`: short display label;
- `text`: searchable body or summary;
- `updated_at`: source update timestamp;
- `metadata_json`: small kind-specific data needed for ranking and display.

Large document bodies may be chunked later, but P0 treats `Document` as a
single node and leaves chunking to the search phase.

## Edge Taxonomy

Initial edge kinds:

| Kind | From | To | Meaning |
|------|------|----|---------|
| `Contains` | `Project` | `Task`, `Plan`, `Document` | project owns item |
| `PlanContains` | `Plan` | `Task` | task is attached to plan |
| `ParentPlan` | `Plan` | `Plan` | child plan belongs under parent |
| `CommentOn` | `Comment` | `Task` | comment was written on task |
| `DocumentMentions` | `Document` | any node | explicit reference parsed from document text |
| `CommentMentions` | `Comment` | any node | explicit reference parsed from comment body |
| `Blocks` | `Task` | `Task` | canonical active task relation |
| `WasBlocking` | `Task` | `Task` | canonical historical blocker relation |
| `RelatesTo` | `Task` | `Task` | canonical task relation |
| `Duplicates` | `Task` | `Task` | canonical task relation |

Edges preserve canonical direction. Query APIs may expose bidirectional
traversal for symmetric relations, but the stored edge remains directed.

All edges store:

- `from_id`;
- `to_id`;
- `kind`;
- `source_event_seq` or projection version when available;
- `metadata_json` for ordering, relation id, mention span, or rank hints.

## Query Shape

P1 should expose at least these graph operations:

- `context(node_id, limit)`: immediate neighborhood plus ranking metadata;
- `related(node_id, depth, limit)`: capped breadth-first traversal;
- `impact(node_id)`: downstream tasks/plans affected through `Blocks`,
  `PlanContains`, and ownership edges;
- `status()`: index version, event lag, size, and last error.

Search integration is out of P0 but the graph should be ready for FTS and later
semantic reranking.

## Human Log

No Human Log document format change is required for P0. WorkspaceGraph will read
Human Log documents as ordinary `Document` nodes. Future auto-append behavior
may make Human Log entries richer, but graph indexing must not depend on that
feature.

## Non-Goals

- Automatically inferring new `Blocks` edges from text or graph proximity.
  `Blocks` remains an explicit user/agent action because it changes execution
  semantics.
- Cross-workspace graph traversal. WorkspaceGraph is scoped to one workspace
  database and one sidecar index.
- Replacing canonical task relations. The graph mirrors relations; it does not
  own them.
- Storing authoritative task, plan, document, or comment state in the sidecar.
- Building semantic embeddings in P0.
- Guaranteeing perfect mention extraction. Missing a mention should degrade
  retrieval quality, not correctness.

## Consequences

The sidecar adds an operational component: migrations, reindexing, lag metrics,
and repair tooling are required. This is acceptable because the component is
derived state and can be rebuilt.

Core TaskAgent commands stay focused on event emission and canonical
projections. WorkspaceGraph can iterate quickly on retrieval, ranking, and
index structure without forcing task storage migrations for every experiment.
