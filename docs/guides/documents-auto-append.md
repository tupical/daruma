# Auto-append into Interview / Human Log

Every project gets two auto-created documents (`CreateProject` emits
them): **Interview** — the AI/agent activity log — and **Human Log** —
a human-readable milestone feed. The server appends lines to them
automatically; the feature is per-project toggleable and **ON by
default** (including projects created before the setting existed).

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

MCP (full profile): `taskagent_project_settings_get`,
`taskagent_project_settings_update { project_id, interview?, human_log? }`.

The state is event-sourced (`ProjectSettingsChanged`), so other clients
get the change in realtime over WS, and the `project_settings`
projection (migration 0034) rebuilds from the log. A missing row means
defaults — both toggles ON.

## Out of scope (separate features)

Custom line templates, auto-archiving old entries, export to external
systems, localization of line templates (current templates are
English).
