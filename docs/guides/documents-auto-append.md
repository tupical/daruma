# Auto-append into Interview / Human Log

The server appends activity lines to two kinds of project documents:
**Interview** (`kind: interview`) — the AI/agent activity log — and
**Human Log** (`kind: human_log`) — a human-readable milestone feed. The
feature is per-project toggleable and **ON by default** (including
projects created before the setting existed).

## The log documents must already exist

`CreateProject` creates the project bare: it does **not** create
Interview or Human Log documents (older releases did). Auto-append only
writes into a document that already exists, so a project without them
logs nothing — silently, the toggles stay ON.

To enable logging, create the document(s) yourself — MCP
`daruma_doc_create { project_id, kind, title, task_id }` or
`POST /v1/documents { "new_doc": { project_id, kind, title, task_id } }`. Like
every live document it must be anchored to a non-terminal task
(`task_id` is required).

- **Target.** For each event the server picks the **oldest non-archived**
  document of the matching kind in the project (`created_at`, then `id`).
  The title is not used; extra documents of the same kind are ignored.
- **Anchor lifecycle.** When the anchor task is cancelled — or the task
  is gone and the orphan sweep runs — the document is archived and
  appends move to the next-oldest document of that kind, or stop if
  there is none. When the anchor task is done the document becomes
  `frozen`, is not archived and still receives appends.
- **A frozen log is never rotated out automatically.** The cancel
  cascade and the orphan sweep only close live (`draft`/`active`)
  documents; they leave a `frozen` one as it is, even when its anchor is
  later cancelled or deleted. It stays the oldest non-archived target
  and keeps receiving appends. To move appends to a newer document,
  archive the frozen one explicitly — MCP
  `daruma_doc_archive { document_id }` (full profile) or
  `POST /v1/documents/{id}/archive`.

## What lands where

| Event | Actor | Log | Line shape |
|-------|-------|-----|-----------|
| TaskCreated | agent | Interview | `[2026-06-09T21:00:00Z] agent=executor-1 action=task_created target=tsk_… "Title"` |
| TaskCreated | user | Human Log | `2026-06-09 21:00 — Created task 'Title'` |
| TaskStatusChanged | agent | Interview | `[ts] agent=… action=status_changed target=tsk_… Todo->Done` |
| TaskStatusChanged | user | Human Log | `2026-06-09 21:00 — Task 'Title' status: Todo → Done` |
| RunStarted / RunCompleted / RunAborted | any | Interview | `[ts] agent=… action=run_started target=run_… plan=pln_…` |
| RunNoteAppended | any | Interview | `[ts] agent=… action=run_note target=run_… <first 120 chars>` |
| PlanStatusChanged → completed | any | Human Log | `2026-06-09 21:00 — Plan 'Title' completed` |
| ProjectUpdated (rename) | any | Human Log | `2026-06-09 21:00 — Project renamed to 'New'` |

Document and settings events never trigger appends (no recursion), and
appends are best-effort: a logging failure never fails the command.
Agent sessions are not project-scoped and are therefore not logged.

## Settings

```text
GET   /v1/projects/{id}/settings
PATCH /v1/projects/{id}/settings   { "auto_append": { "interview": false } }
```

MCP (full profile): `daruma_project_settings_get`,
`daruma_project_settings_update { project_id, interview?, human_log? }`.

The state is event-sourced (`ProjectSettingsChanged`), so other clients
get the change in realtime over WS, and the `project_settings`
projection (migration 0034) rebuilds from the log. A missing row means
defaults — both toggles ON.

The same endpoint carries two more keys, each replaced whole (`null`
removes it, an absent key leaves it unchanged): `intake_source` — the
plan-source policy (ADR-0009) — and `audit` — a JSON object of audit
thresholds read by the hosted audit engine (OSS only requires an object;
the host may plug a stricter validator via
`CommandHandler::with_audit_settings_validator`). MCP:
`daruma_project_settings_update { project_id, intake_source?, audit? }`.

## Out of scope (separate features)

Custom line templates, auto-archiving old entries, export to external
systems, localization of line templates (current templates are
English).
