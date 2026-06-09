# taskagent override — task & plan persistence

This project uses the **taskagent** MCP server as the single source of
truth for tasks and plans. The taskagent-claude Cursor plugin manages
this block; do not hand-edit between the markers.

**For any OMC skill that would normally write under `.omc/` plans**
(`/plan`, `/ultragoal`, `/autopilot`, `/ralph`, `/ultrawork`,
`/ralplan`, `/team`):

- **Do not create or update** `.omc/plans/`, `.omc/ultragoal/`, or
  `.omc/state/plans*`. Those paths are reserved as no-write for plan
  authoring in this workspace.
- **Author plans via taskagent first:** `taskagent_workspace_info` →
  `taskagent_create` → `taskagent_plan_create` →
  `taskagent_plan_add_task`. OMC may still execute, but the plan it
  consumes is fetched via `taskagent_plan_get` /
  `taskagent_plan_next_task`.
- **OMC logs, state/sessions, notepad, and research artifacts**
  (`.omc/logs/`, `.omc/state/sessions/`, `.omc/notepad.md`,
  `.omc/research/`) remain untouched by this rule — only plan
  persistence is redirected.

If `taskagent_healthz` fails, surface that to the user and ask them to
start the taskagent server. Do not silently fall back to `.omc/plans/`.