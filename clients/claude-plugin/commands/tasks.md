---
description: Show open daruma tasks for the active project as a compact markdown table.
---

The user invoked `/daruma-claude:tasks`.

Drive the daruma MCP server (do not invent IDs, do not write to
`.omc/plans/`).

## Steps

1. Resolve project:
   - `daruma_workspace_info` → use `default_project` if set.
   - Else `daruma_project_list` → pick first. If none, say
     "no projects yet — `daruma_project_create` first" and stop.

2. Fetch tasks server-side (don't filter locally):
   - `daruma_list` with `project_id = <resolved>`,
     `status = ["inbox", "todo", "in_progress"]`, limit ~50.
   - **Never** use the archive-wide all-status listing unless the user
     explicitly asked for the full archive; it is token-heavy.

3. Render exactly this format:

   ```
   ## <project title> — <N> open tasks

   | # | Status | Pri | Title | Plan |
   |---|--------|-----|-------|------|
   | 1 | 🟢 in_progress | p1 | <truncated title> | <plan_short> |
   …
   ```

   - Status emoji: 📥 inbox, ⬜ todo, 🟢 in_progress, ✅ done.
   - Title truncated to 60 chars with `…`.
   - `Plan` shows last 8 chars of `plan_id` or `—` if none.

4. If >30 rows: show first 30, footer `…and <N> more — narrow with
   /daruma-claude:mine or filter by status`.

5. On `daruma_list` error: print the error verbatim and stop.

6. Read-only — do not transition any task in this command.
