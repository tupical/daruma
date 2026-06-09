---
name: setup
description: Print install instructions for any missing taskagent-claude dependencies. Triggered by /taskagent-claude:setup or taskagent-claude setup from the shell.
---

# taskagent-claude: setup

This skill **does not run installers**. It detects what's missing and prints the official install commands so the user decides.

## Step 1 — Detect

```
if [ -n "$CLAUDE_PLUGIN_ROOT" ]; then node "$CLAUDE_PLUGIN_ROOT/bin/taskagent-claude.mjs" setup; else taskagent-claude setup; fi
```

The detector prints install hints for each missing dependency. Show the output verbatim.

## Step 2 — Self-host TaskAgent

Credentials may live at **`~/.agents/taskagent/credentials.json`** (Windows: `%USERPROFILE%\.agents\taskagent\credentials.json`). Override the directory with `TASKAGENT_AGENT_DIR`.

```bash
git clone https://github.com/tupical/taskagent
cd taskagent
cargo build --release -p taskagent-server -p taskagent-cli
./target/release/taskagent-server
claude mcp add taskagent -- ./target/release/taskagent-mcp
```

Relevant env vars for `taskagent-mcp`:

- `TASKAGENT_API_URL` — server base (default `http://localhost:8080` when no credentials).
- `TASKAGENT_TOKEN` — bearer token.
- `TASKAGENT_WORKSPACE_ID` — optional workspace UUID for scoped deployments.

## Step 3 — oh-my-claudecode (needed for `taskagent-claude start`)

```
npm i -g oh-my-claude-sisyphus@latest
omc setup
```

Also enable native teams in `~/.claude/settings.json`:

```
{ "env": { "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS": "1" } }
```

Requires Node.js ≥ 20.

## Step 4 — Updates

`taskagent-claude update` checks `taskagent-claude` (npm) and `omc`. taskagent itself has to be pulled and rebuilt manually:

```
cd <taskagent-repo> && git pull && cargo build --release -p taskagent-server -p taskagent-cli
```

## Step 5 — Re-verify

After install, run `taskagent-claude doctor`. Do not assume install succeeded based on a CLI exit code.
