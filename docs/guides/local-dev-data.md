# Local development data layout

All local server state lives under **`~/.agents/taskagent/data/`** (override with
`TASKAGENT_DATA_DIR`). MCP client config stays in the parent directory.

```text
~/.agents/taskagent/
  workspaces.json    # repo path → project id (MCP)
  credentials.json   # remote/self-host profiles (CLI)
  data/              # taskagent-server (canonical)
    taskagent.sqlite
    workspacegraph.sqlite
    bootstrap.token
```

`taskagent-web` does not choose the database — only the `taskagent-server`
process on `:8080` does.

## Defaults in code

| Component | `TASKAGENT_DATA_DIR` unset |
|-----------|----------------------------|
| `taskagent-server` | `~/.agents/taskagent/data` |
| `just server` | same (via `Justfile`) |
| `taskagent` CLI bootstrap | same (`taskagent_mcp::paths::data_dir`) |

Override only when you need an isolated copy (e.g. tests):

```bash
export TASKAGENT_DATA_DIR=/path/to/custom/data
```

Do **not** use ad-hoc paths such as `<repo>/data` or `/tmp/taskagent-*` for
normal development — agents should always start the server with the canonical
directory above.

## Local web stack

```bash
# Terminal 1 — API (from taskagent repo)
just server

# Terminal 2 — UI (from taskagent-web)
NO_COLOR=false trunk serve
```

Open (after first server start created `bootstrap.token`):

```text
http://127.0.0.1:5174/web/?token=$(cat ~/.agents/taskagent/data/bootstrap.token)
http://127.0.0.1:5174/workspaces
```

Or use `taskagent-web/scripts/dev-stack.sh`.

## Agents (Cursor, Codex, …)

1. Start `taskagent-server` **without** inventing a new data path.
2. Token: `~/.agents/taskagent/data/bootstrap.token`
3. MCP may still use Remote via `TASKAGENT_API_URL` — that is a **different**
   database from local `taskagent.sqlite`.

## Legacy locations

| Old path | Action |
|----------|--------|
| `<taskagent-repo>/data/` | Copy into `~/.agents/taskagent/data/` once, then stop using repo `data/` |
| `/tmp/taskagent-web-local/` | Same — merge if needed, then delete |

```bash
mkdir -p ~/.agents/taskagent/data
cp -an /path/to/old/data/. ~/.agents/taskagent/data/   # no clobber (-n)
```

Restart the server (no `TASKAGENT_DATA_DIR` override required).
