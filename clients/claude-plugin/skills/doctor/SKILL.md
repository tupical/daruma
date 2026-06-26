---
name: doctor
description: Detect whether tupical/daruma and oh-my-claudecode are installed and the daruma MCP server is reachable. Triggered by /daruma-claude:doctor or daruma-claude doctor from the shell.
---

# daruma-claude: doctor

Run the bundled detector and surface its output verbatim. Do not install anything.

## Step 1 — Run the detector

```
if [ -n "$CLAUDE_PLUGIN_ROOT" ]; then node "$CLAUDE_PLUGIN_ROOT/bin/daruma-claude.mjs" doctor; else daruma-claude doctor; fi
```

## Step 2 — What the detector checks

- `omc` CLI / `oh-my-claude-sisyphus` npm package.
- `daruma-mcp` (or `daruma`) CLI on PATH.
- `claude mcp list` showing the `daruma:` shim as `Connected`.
- `~/.agents/daruma/credentials.json` (or `DARUMA_AGENT_DIR`) — optional local/self-host profile with token.
- HTTP `GET $DARUMA_API_URL/v1/healthz` (URL from credentials when present, else `http://localhost:8080`).

Positive results are cached for 30s in `~/.cache/daruma-claude/doctor.json`. Negative results are never cached.

## Step 3 — In-session MCP sanity check (only if running inside Claude Code)

```
ToolSearch query: "+daruma workspace" max_results: 3
```

Missing `mcp__daruma_*` tools despite a READY verdict → confirm the user restarted Claude Code after `claude mcp add daruma ...`.

## Step 4 — Verdict

- `READY` → user can run `daruma-claude start "<task>"`.
- `NOT READY` → list each missing piece with the install hint from Step 1's output. Do not synthesize a "best-effort" yes.
