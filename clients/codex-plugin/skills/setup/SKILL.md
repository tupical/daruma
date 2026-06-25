---
name: setup
description: Print install instructions for any missing daruma-claude dependencies. Triggered by /daruma-claude:setup or daruma-claude setup from the shell.
---

# daruma-claude: setup

This skill **does not run installers**. It detects what's missing and prints the official install commands so the user decides.

## Step 1 — Detect

```
if [ -n "$CLAUDE_PLUGIN_ROOT" ]; then node "$CLAUDE_PLUGIN_ROOT/bin/daruma-claude.mjs" setup; else daruma-claude setup; fi
```

The detector prints install hints for each missing dependency. Show the output verbatim.

## Step 2 — Self-host Daruma

Credentials may live at **`~/.agents/daruma/credentials.json`** (Windows: `%USERPROFILE%\.agents\daruma\credentials.json`). Override the directory with `DARUMA_AGENT_DIR`.

```bash
git clone https://github.com/tupical/daruma
cd daruma
cargo build --release -p daruma-server -p daruma-cli
./target/release/daruma-server
claude mcp add daruma -- ./target/release/daruma-mcp
```

Relevant env vars for `daruma-mcp`:

- `DARUMA_API_URL` — server base (default `http://localhost:8080` when no credentials).
- `DARUMA_TOKEN` — bearer token.
- `DARUMA_WORKSPACE_ID` — optional workspace UUID for scoped deployments.

## Step 3 — oh-my-claudecode (needed for `daruma-claude start`)

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

`daruma-claude update` checks `daruma-claude` (npm) and `omc`. daruma itself has to be pulled and rebuilt manually:

```
cd <daruma-repo> && git pull && cargo build --release -p daruma-server -p daruma-cli
```

## Step 5 — Re-verify

After install, run `daruma-claude doctor`. Do not assume install succeeded based on a CLI exit code.
