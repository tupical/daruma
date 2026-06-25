# daruma-codex

Codex companion commands and skills for Daruma.

Daruma is your last next task manager: crafted for speed and collaboration
with humans and AI. This package teaches Codex how to use Daruma as an
agent-native workflow store instead of inventing a local task tracker for each
session.

## What it contains

- Slash commands for planning, starting, listing, and claiming Daruma tasks.
- Skills that map Codex work sessions to Daruma projects, plans, and task
  updates.
- Setup and doctor helpers for local MCP wiring.
- `daruma-codex init` — drops a managed policy block into `AGENTS.md`
  (includes the rule: ask the user before `daruma_list` /
  `daruma_plan_list` with `status=all`).

Run once per repo:

```bash
daruma-codex init
```

## Russian

See [README.ru.md](README.ru.md).
