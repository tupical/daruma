---
name: doctor
description: Detect whether tupical/taskagent and oh-my-claudecode are installed and the taskagent MCP server is reachable. Triggered by /taskagent-claude:doctor or taskagent-claude doctor from the shell.
---

# taskagent-claude: doctor

Run the bundled detector and surface its output verbatim. Do not install anything.

## Step 1 — Run the detector

```
if [ -n "$CLAUDE_PLUGIN_ROOT" ]; then node "$CLAUDE_PLUGIN_ROOT/bin/taskagent-claude.mjs" doctor; else taskagent-claude doctor; fi
```

## Step 2 — What the detector checks

- `omc` CLI / `oh-my-claude-sisyphus` npm package.
- `taskagent-mcp` (or `taskagent`) CLI on PATH.
- `claude mcp list` showing the `taskagent:` shim as `Connected`.
- `~/.agents/taskagent/credentials.json` (or `TASKAGENT_AGENT_DIR`) — optional local/self-host profile with token.
- HTTP `GET $TASKAGENT_API_URL/v1/healthz` (URL from credentials when present, else `http://localhost:8080`).

Positive results are cached for 30s in `~/.cache/taskagent-claude/doctor.json`. Negative results are never cached.

## Step 3 — In-session MCP sanity check (only if running inside Claude Code)

```
ToolSearch query: "+taskagent workspace" max_results: 3
```

Missing `mcp__taskagent_*` tools despite a READY verdict → confirm the user restarted Claude Code after `claude mcp add taskagent ...`.

## Step 4 — Verdict

- `READY` → user can run `taskagent-claude start "<task>"`.
- `NOT READY` → list each missing piece with the install hint from Step 1's output. Do not synthesize a "best-effort" yes.
