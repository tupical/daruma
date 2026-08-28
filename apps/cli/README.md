# `daruma` — terse CLI

`apps/cli/` builds a single binary, `daruma`, that is a thin wrapper
over `crates/mcp/src/client.rs` (i.e. the same HTTP surface the MCP
server hops through). It exists for two callers:

1. **Humans** at a shell who want a quick "what's next?" without
   touching MCP/Claude Desktop.
2. **Agents-through-shell** — anything that can spawn a subprocess and
   pipe `--json` (CI scripts, ralph loops, plain `bash`).

The CLI does **not** replicate every MCP tool — only the verbs that hurt
the most when typed by hand: `next`, `show`, `done`, `list`, `history` —
plus `install` (integration setup) and `mcp` (the stdio MCP server).

## Build & install

```bash
cargo build --release -p daruma-cli
# binary at target/release/daruma
```

## Configure

```bash
export DARUMA_API_URL=http://localhost:8080
export DARUMA_TOKEN=ta_svc_xxxxxxxx
# Optional — scopes `next` / `list` to a single project:
export DARUMA_PROJECT_ID=<project_id>
# Optional — workspace key (MCP persists defaults; CLI uses env only):
export DARUMA_WORKSPACE="$PWD"
```

MCP client disk layout (`daruma mcp`): [docs/guides/mcp-client.md](../../docs/guides/mcp-client.md).

You can also pass `--api-url` and `--token` per-invocation. They override
the env.

## MCP config

For Cursor remote MCP, print a ready-to-merge `mcp.json` snippet:

```bash
daruma install --print-config cursor
```

The snippet uses `<DARUMA_API_URL>/v1/mcp` and adds an
`Authorization: Bearer ...` header when a token is available from env or
`~/.agents/daruma/credentials.json`.

To persist a self-host/local profile non-interactively:

```bash
DARUMA_API_URL=http://127.0.0.1:8080 \
DARUMA_TOKEN=ta_svc_... \
daruma install --mode self-host -y
```

For local mode, `DARUMA_TOKEN` may be omitted when
`$DARUMA_DATA_DIR/bootstrap.token` exists:

```bash
DARUMA_DATA_DIR=~/.agents/daruma/data \
daruma install --mode local -y
```

## Verbs

```bash
# Next claim-ready task in the current project (todo → in_progress → inbox).
daruma next

# Show one task + its comments.
daruma show <task_id>

# Mark a task done.
daruma done <task_id>

# List open tasks in the current project (status filter is required).
daruma list --status active

# Filter by a specific status.
daruma list --status todo
daruma list --status in_progress

# Full archive (including done/cancelled).
daruma list --status all

# Ignore the workspace default project scope.
daruma list --status active --project-id all

# Show version history for a task or document.
daruma history task <task_id>
daruma history document doc_<doc_id> --limit 20
```

## `--json` for agents

Every command accepts `--json` and writes a single JSON value to stdout —
nothing else (logs go to stderr). The shape mirrors what the server
returns:

```bash
daruma next --json                 # one task object, or `null`
daruma show <id> --json            # { "task": {...}, "comments": [...] }
daruma list --status todo --json   # array of task objects
daruma done <id> --json            # MutationResponse from the server
daruma history task <id> --json    # array of version records
```

This is the integration contract — feel free to script against it.

## Why this exists (and what it deliberately is not)

Design goal: terse verbs, table output, `--json` flag for
agents-through-shell. The CLI is a path-of-least-resistance entry point
for one-off ops; it is **not** trying to be a full TUI, and it does not
duplicate workflow tools (planning, claims, run lifecycle) — those stay
in MCP where the schemas already live.
