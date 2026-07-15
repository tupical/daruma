# daruma override — task & plan persistence

This project uses the **daruma** MCP server as the single source of
truth for tasks and plans. The daruma-claude Cursor plugin manages
this block; do not hand-edit between the markers.

**For any OMC skill that would normally write under `.omc/` plans**
(`/plan`, `/ultragoal`, `/autopilot`, `/ralph`, `/ultrawork`,
`/ralplan`, `/team`):

- **Do not create or update** `.omc/plans/`, `.omc/ultragoal/`, or
  `.omc/state/plans*`. Those paths are reserved as no-write for plan
  authoring in this workspace.
- **Author plans via daruma first:** `daruma_workspace_info` →
  `daruma_plan_materialize` (the plan with its tasks, one atomic call). OMC may still execute, but the plan it
  consumes is fetched via `daruma_plan_get` /
  `daruma_plan_next_task`.
- **OMC logs, state/sessions, notepad, and research artifacts**
  (`.omc/logs/`, `.omc/state/sessions/`, `.omc/notepad.md`,
  `.omc/research/`) remain untouched by this rule — only plan
  persistence is redirected.

If `daruma_healthz` fails, surface that to the user and ask them to
start the daruma server. Do not silently fall back to `.omc/plans/`.