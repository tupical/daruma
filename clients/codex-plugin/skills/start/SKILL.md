---
name: start
description: "Run taskagent-claude's full pipeline: parse the task, optionally decompose via AI, execute each subtask through omc team, write artifacts as taskagent comments. Triggered by /taskagent-claude:start \"<task>\" or taskagent-claude start \"<task>\" from the shell."
---

# taskagent-claude: start

Drive the end-to-end pipeline: parse → (optionally) decompose → execute via `omc team` → comment artifacts back onto the taskagent task. This skill orchestrates; it does not implement the steps inline.

## Step 1 — Preflight

```
taskagent-claude doctor --quiet
```

If preflight fails, abort and show the full `taskagent-claude doctor` report so the user can see what's missing. Do not proceed with `start`.

## Step 2 — Run the pipeline

```
taskagent-claude start "<task>" [--plan] [--workers N] [--max-retries M] [--agent claude|codex|gemini] [--project ID] [--yes]
```

Flag summary:

- `--plan` — enable AI decomposition of the task into subtasks before execution.
- `--workers N` — concurrent `omc team` workers per subtask.
- `--max-retries M` — per-subtask retry budget.
- `--agent T` — executor backend (`claude`, `codex`, or `gemini`).
- `--project ID` — taskagent project to attach to.
- `--yes` — auto-confirm prompts (use for non-interactive runs).

## Step 3 — Exit code interpretation

- `0` — pipeline finished, task complete.
- `1` — a task failed, or preflight failed.
- `130` — cancelled (SIGINT / user abort).

## Step 4 — Logs

If something looks off, check:

- `.omc/logs/taskagent-mcp.stderr.log` — taskagent MCP shim stderr.
- `.omc/logs/omc-team.stderr.log` — `omc team` executor stderr.
