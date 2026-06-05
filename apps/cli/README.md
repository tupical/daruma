# `taskagent` — terse CLI

`apps/cli/` builds a single binary, `taskagent`, that is a thin wrapper
over `crates/mcp/src/client.rs` (i.e. the same HTTP surface the MCP
server hops through). It exists for two callers:

1. **Humans** at a shell who want a quick "what's next?" without
   touching MCP/Claude Desktop.
2. **Agents-through-shell** — anything that can spawn a subprocess and
   pipe `--json` (CI scripts, ralph loops, plain `bash`).

The CLI does **not** replicate every MCP tool — only the verbs that hurt
the most when typed by hand: `next`, `show`, `done`, `list`, `history`.

## Build & install

```bash
cargo build --release -p taskagent-cli
# binary at target/release/taskagent
```

## Configure

```bash
export TASKAGENT_API_URL=http://localhost:8080
export TASKAGENT_TOKEN=ag_dev_xxxxxxxx
# Optional — scopes `next` / `list` to a single project:
export TASKAGENT_PROJECT_ID=01939e35-...
# Optional — workspace key (MCP persists defaults; CLI uses env only):
export TASKAGENT_WORKSPACE="$PWD"
```

MCP client disk layout (`taskagent-mcp`): [docs/guides/mcp-client.md](../../docs/guides/mcp-client.md).

You can also pass `--api-url` and `--token` per-invocation. They override
the env.

## MCP config

For Cursor remote MCP, print a ready-to-merge `mcp.json` snippet:

```bash
taskagent install --print-config cursor
```

The snippet uses `<TASKAGENT_API_URL>/v1/mcp` and adds an
`Authorization: Bearer ...` header when a token is available from env or
`~/.agents/taskagent/credentials.json`.

To persist a self-host/local profile non-interactively:

```bash
TASKAGENT_API_URL=http://127.0.0.1:8080 \
TASKAGENT_TOKEN=ta_svc_... \
taskagent install --mode self-host -y
```

For local mode, `TASKAGENT_TOKEN` may be omitted when
`$TASKAGENT_DATA_DIR/bootstrap.token` exists:

```bash
TASKAGENT_DATA_DIR=~/.agents/taskagent/data \
taskagent install --mode local -y
```

## Verbs

```bash
# Next claim-ready task in the current project (todo → in_progress → inbox).
taskagent next

# Show one task + its comments.
taskagent show 019e351b-3f3a-7850-a0bd-85135c0b24d0

# Mark a task done.
taskagent done 019e351b-3f3a-7850-a0bd-85135c0b24d0

# List open tasks in the current project (status filter is required).
taskagent list --status active

# Filter by a specific status.
taskagent list --status todo
taskagent list --status in_progress

# Full archive (including done/cancelled).
taskagent list --status all

# Ignore the workspace default project scope.
taskagent list --status active --project-id all

# Show version history for a task or document.
taskagent history task 019e351b-3f3a-7850-a0bd-85135c0b24d0
taskagent history document doc_019e351b-3f3a-7850-a0bd-85135c0b24d0 --limit 20
```

## `--json` for agents

Every command accepts `--json` and writes a single JSON value to stdout —
nothing else (logs go to stderr). The shape mirrors what the server
returns:

```bash
taskagent next --json                 # one task object, or `null`
taskagent show <id> --json            # { "task": {...}, "comments": [...] }
taskagent list --status todo --json   # array of task objects
taskagent done <id> --json            # MutationResponse from the server
taskagent history task <id> --json    # array of version records
```

This is the integration contract — feel free to script against it.

## Why this exists (and what it deliberately is not)

Roadmap §3.8.11 / CTM B.5: "terse verbs, table output, `--json` flag for
agents-through-shell". The CLI is a path-of-least-resistance entry point
for one-off ops; it is **not** trying to be a full TUI, and it does not
duplicate workflow tools (planning, claims, run lifecycle) — those stay
in MCP where the schemas already live.
