---
name: start
description: Run daruma-claude's full pipeline: parse the task, record it as a daruma plan, execute it through omc team, write artifacts as daruma comments. Triggered by /daruma-claude:start "<task>" or daruma-claude start "<task>" from the shell.
---

# daruma-claude: start

Drive the end-to-end pipeline: parse → materialize the root plan → execute via `omc team` → comment artifacts back onto the daruma task. This skill orchestrates; it does not implement the steps inline.

## Step 1 — Preflight

```
daruma-claude doctor --quiet
```

If preflight fails, abort and show the full `daruma-claude doctor` report so the user can see what's missing. Do not proceed with `start`.

## Step 2 — Run the pipeline

```
daruma-claude start "<task>" [--plan] [--workers N] [--max-retries M] [--agent claude|codex|gemini] [--project ID] [--yes]
```

Flag summary:

- `--plan` — execute the root task through the plan loop (daruma run + claim) instead of directly. It does not decompose and needs no AI provider; for several tasks author a plan first and use `/daruma-claude:team-from-plan`.
- `--workers N` — concurrent `omc team` workers per task.
- `--max-retries M` — per-task retry budget.
- `--agent T` — executor backend (`claude`, `codex`, or `gemini`).
- `--project ID` — daruma project to attach to.
- `--yes` — auto-confirm prompts (use for non-interactive runs).

## Step 3 — Exit code interpretation

- `0` — pipeline finished, task complete.
- `1` — a task failed, or preflight failed.
- `130` — cancelled (SIGINT / user abort).

## Step 4 — Logs

If something looks off, check:

- `.omc/logs/daruma-mcp.stderr.log` — daruma MCP shim stderr.
- `.omc/logs/omc-team.stderr.log` — `omc team` executor stderr.
