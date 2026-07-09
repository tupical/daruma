---
name: team-from-plan
description: Execute an existing daruma plan wave-by-wave through omc team workers. Triggered by /daruma-claude:team-from-plan <plan_id> or daruma-claude team-from-plan <plan_id> from the shell.
---

# daruma-claude: team-from-plan

Execute an already-authored daruma plan via `daruma_plan_fanout` waves. This skill orchestrates; it does not create or decompose plans.

## Step 1 — Preflight

```
daruma-claude doctor --quiet
```

If preflight fails, abort and show the full `daruma-claude doctor` report so the user can see what's missing. Do not proceed with `team-from-plan`.

## Step 2 — Run the pipeline

```
daruma-claude team-from-plan <plan_id> [--workers N] [--max-retries M] [--agent claude|codex|gemini] [--yes]
```

Flag summary:

- `--workers N` — concurrent plan tasks per wave.
- `--max-retries M` — per-task retry budget.
- `--agent T` — executor backend (`claude`, `codex`, or `gemini`).
- `--yes` — auto-confirm prompts (use for non-interactive runs).

## Step 3 — Exit code interpretation

- `0` — all executed plan tasks finished.
- `1` — one or more tasks failed, or preflight failed.
- `130` — cancelled (SIGINT / user abort).

## Step 4 — Logs

If something looks off, check:

- `.omc/logs/daruma-mcp.stderr.log` — daruma MCP shim stderr.
- `.omc/logs/omc-team.stderr.log` — `omc team` executor stderr.
