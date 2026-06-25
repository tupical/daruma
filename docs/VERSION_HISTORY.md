# Version History Contract

This document defines the immutable version record contract for task and
document changes. Storage schema, writer implementation, read APIs, rollback,
and UI must preserve this contract unless a new architecture decision
explicitly supersedes it.

## Goals

- Preserve an audit-grade history for every task and document mutation.
- Make history useful to humans and agents without replaying the event log.
- Keep rollback append-only: reverting creates a new version, never rewrites
  existing history.
- Let storage and API implementations share one entity-agnostic record shape.

## Entity Scope

Version history covers mutable user-facing entities:

| Entity type | Entity id source | Snapshot payload |
|---|---|---|
| `task` | `TaskId` (`tsk_*`) | serialized `daruma_domain::Task` |
| `document` | `DocumentId` (`doc_*`) | serialized `daruma_domain::Document` |

Other projections such as activity rows, comments, plans, runs, and relations
remain event-log/audit-log backed unless a later task adds them explicitly.

## Record Shape

Implementations should use a shared logical record named `EntityVersion`.
The Rust type and SQL table names may differ, but public APIs must expose the
same fields.

| Field | Type | Required | Meaning |
|---|---|---:|---|
| `id` | `VersionId` (`ver_*` UUIDv7) | yes | Stable id of this version record. |
| `entity_type` | `task | document` | yes | Discriminator for the versioned entity. |
| `entity_id` | string id | yes | `tsk_*` or `doc_*` id matching `entity_type`. |
| `version_number` | positive integer | yes | Per-entity monotonic number, starts at `1`. |
| `actor` | `Actor` JSON | yes | Actor from the command/event that caused the version. |
| `event_type` | string | yes | Stable event kind, e.g. `task_updated`, `document_content_appended`. |
| `reason` | string or null | yes | Optional human/agent reason; null when not supplied. |
| `source_event_id` | `EventId` (`evt_*`) or null | yes | Event that produced the entity state, when available. |
| `source_event_seq` | integer or null | yes | Global event sequence for ordering/debugging. |
| `created_at` | UTC timestamp | yes | Time the version record was created. |
| `before` | JSON object or null | yes | Entity snapshot before the mutation; null for first version. |
| `after` | JSON object or null | yes | Entity snapshot after the mutation; null only for terminal hard-delete if ever added. |
| `diff` | JSON object | yes | Structured machine-readable diff. |
| `changed_fields` | string array | yes | Top-level entity fields changed by this version. |
| `summary` | string | yes | Short agent-readable summary of the change. |

`VersionId` is a new shared id type with display prefix `ver_`. It follows the
same UUIDv7 newtype conventions as `TaskId`, `DocumentId`, and `EventId`.

## Storage Mapping

SQLite uses a shared `entity_versions` table (`0020_entity_versions.sql`).
JSON fields are stored as `TEXT`, matching the rest of `daruma-storage`.

Required columns map one-to-one to the public record:

- `id`, `entity_type`, `entity_id`, `version_number`
- `actor_json` for public `actor`
- `event_type`, `reason`, `source_event_id`, `source_event_seq`, `created_at`
- `before_json`, `after_json`, `diff_json`, `changed_fields_json`, `summary`

Storage also keeps denormalized actor columns for indexed audit queries:
`actor_kind`, `actor_id`, and `actor_name`. These columns are internal storage
helpers and do not replace `actor_json` in public APIs.

Storage stores `entity_id` in display form (`tsk_*` / `doc_*`). Public read
APIs accept either display form or the raw UUID form emitted by existing task
and document JSON payloads; the storage reader normalizes raw ids before
querying.

Required indexes:

- unique `(entity_type, entity_id, version_number)`
- `(entity_type, entity_id, version_number DESC)` for entity history
- `(entity_type, entity_id, created_at DESC, id DESC)` for timeline pagination
- `(created_at DESC, id DESC)` for latest-changes feeds
- `(actor_kind, actor_id, created_at DESC)` and
  `(actor_kind, actor_name, created_at DESC)` for actor filters
- unique `(entity_type, entity_id, source_event_id)` where `source_event_id`
  is not null, for idempotent event replay

## Invariants

1. `(entity_type, entity_id, version_number)` is unique.
2. `version_number` increments by exactly one for each entity.
3. Versions are immutable after insert.
4. Entity state and its version record are written in the same transaction.
5. `after` matches the committed projection state after that transaction.
6. `before` for version `N` equals `after` for version `N - 1`, except for
   explicit repair/backfill jobs that write `reason = "backfill"` or
   `reason = "repair"`.
7. Rollback writes a normal new version with `reason = "rollback"` and a
   `rollback_of_version_id` entry inside `diff.metadata`.
8. Public list APIs sort by `(version_number DESC)` by default and may also
   expose `(created_at, id)` cursor pagination.

## Diff Contract

`diff` is entity-specific but always has this envelope:

```json
{
  "kind": "field_json_patch",
  "fields": {},
  "metadata": {}
}
```

### Task Diffs

Task versions use field-level JSON diffs:

```json
{
  "kind": "field_json_patch",
  "fields": {
    "title": { "before": "Old title", "after": "New title" },
    "status": { "before": "todo", "after": "in_progress" }
  },
  "metadata": {
    "source": "task_updated"
  }
}
```

`changed_fields` is the sorted list of keys under `fields`.

### Document Diffs

Document metadata changes use the same field diff shape. Content changes add a
unified text patch for the `content` field:

```json
{
  "kind": "document_text_patch",
  "fields": {
    "content": {
      "before_hash": "sha256:...",
      "after_hash": "sha256:...",
      "unified_diff": "--- before\n+++ after\n@@ ...\n"
    }
  },
  "metadata": {
    "source": "document_content_replaced"
  }
}
```

Large content snapshots remain in `before` and `after` for MVP simplicity.
Future storage may deduplicate snapshots internally, but APIs still expose the
logical snapshots.

## Summaries

`summary` is deterministic application text, not an LLM-only field. It should
stay short enough for timeline UIs and agent context windows:

- `Task title changed: "Old title" -> "New title"`
- `Task status changed: todo -> in_progress`
- `Document content appended: 128 characters`
- `Rollback applied from version 4`

Multiple field changes may use a compact summary such as
`Task updated: title, description, due_at`.

## Read API

HTTP routes are authenticated under `/v1` and legacy root aliases:

| Route | Capability | Meaning |
|---|---|---|
| `GET /v1/history?entity_type=task&entity_id=...&limit=50` | `task:read` or `document:read` by entity type | List one entity timeline, newest first. |
| `GET /v1/history/{version_id}` | matching read capability | Fetch one immutable version record. |
| `GET /v1/history/compare?entity_type=...&entity_id=...&from=1&to=2` | matching read capability | Compare two version numbers for the same entity. |
| `GET /v1/history/latest?limit=50` | any task/document read capability | Latest visible task/document changes. |
| `GET /v1/history/summary?entity_type=...&entity_id=...&limit=50` | matching read capability | Compact agent-readable timeline. |
| `POST /v1/history/{version_id}/rollback` | `task:write` or `document:write` by entity type | Restore the selected snapshot by writing new mutation events. |

MCP tools mirror the HTTP routes:

- `daruma_history_list`
- `daruma_history_get`
- `daruma_history_compare`
- `daruma_history_latest`
- `daruma_history_summary`
- `daruma_history_rollback`

## Rollback

Rollback is append-only. It never deletes, squashes, or rewrites old version
records. `POST /v1/history/{version_id}/rollback` reads the selected version's
`after` snapshot and dispatches ordinary mutation commands:

- task rollback uses `Command::UpdateTask` with the mutable fields from the
  snapshot (`title`, `description`, `status`, `priority`, `due_at`,
  `project_id`);
- document rollback uses `Command::RenameDocument` and/or
  `Command::ReplaceDocumentContent` when the current title/body differ.

The new version record produced by those events is marked with
`reason = "rollback"` and includes
`diff.metadata.rollback_of_version_id = <selected_version_id>`.

Current MVP boundaries:

- task rollback does not restore derived audit timestamps such as
  `created_at`, `updated_at`, `completed_at`, or `updated_event_seq`; those
  remain facts about the new rollback event;
- document rollback restores title/body only, not `archived_at`;
- rolling back to the current state may produce no new version if the emitted
  command causes no projection change.

## Example

```json
{
  "id": "ver_019e867a-2c48-7c80-a605-4a15f26fb7a0",
  "entity_type": "task",
  "entity_id": "tsk_019e867a-1f45-70e1-a1f0-b2e953f4ac12",
  "version_number": 3,
  "actor": { "kind": "agent", "id": "agt_019e867a-1111-7222-8333-444444444444", "name": "codex" },
  "event_type": "task_updated",
  "reason": null,
  "source_event_id": "evt_019e867a-2b7e-7612-9ea6-6ab6a89fe111",
  "source_event_seq": 2641,
  "created_at": "2026-06-02T04:05:00Z",
  "before": {
    "id": "tsk_019e867a-1f45-70e1-a1f0-b2e953f4ac12",
    "title": "Draft spec",
    "status": "todo"
  },
  "after": {
    "id": "tsk_019e867a-1f45-70e1-a1f0-b2e953f4ac12",
    "title": "Draft version history spec",
    "status": "todo"
  },
  "diff": {
    "kind": "field_json_patch",
    "fields": {
      "title": {
        "before": "Draft spec",
        "after": "Draft version history spec"
      }
    },
    "metadata": {
      "source": "task_updated"
    }
  },
  "changed_fields": ["title"],
  "summary": "Task title changed: \"Draft spec\" -> \"Draft version history spec\""
}
```

## Non-Goals For MVP

- Git-like branching, merge commits, tags, or conflict resolution.
- Rewriting, squashing, or deleting history.
- Version records for every projection table.
- LLM-generated long-form summaries as required data.
- External export formats beyond JSON.
- Internal git-style storage. Version history is a first-class database
  contract; future git-like branching/merge/export work must be added as an
  explicit follow-up design rather than inferred from this MVP.
