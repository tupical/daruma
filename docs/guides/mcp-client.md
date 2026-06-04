# MCP client local data (`taskagent-mcp`)

The stdio MCP binary keeps **no** state in the repository. All on-disk
files for the client live under a single directory:

```text
~/.agents/taskagent/
  credentials.json   # optional local/self-host profiles
  workspaces.json    # map: workspace/scope path → default project id
  data/              # server SQLite (default TASKAGENT_DATA_DIR)
    taskagent.sqlite
    bootstrap.token
```

## Environment

| Variable | Purpose |
|----------|---------|
| `TASKAGENT_WORKSPACE` | Workspace key (default: process CWD). Used to infer the nearest configured repo scope in `workspaces.json`. |
| `TASKAGENT_PROJECT_ID` | Overrides the on-disk default for this session. |
| `TASKAGENT_AGENT_DIR` | Override agent data root (default `~/.agents/taskagent`). |
| `TASKAGENT_WORKSPACES_FILE` | Override path to `workspaces.json` only. |
| `TASKAGENT_API_URL` / `TASKAGENT_TOKEN` | Remote server (see [ai-agent.md](ai-agent.md)). When unset, `taskagent-mcp` reads the active profile from `credentials.json`. |
| `TASKAGENT_WORKSPACE_ID` | Optional workspace UUID sent as `X-TaskAgent-Workspace-Id`. If unset, uses `workspace_id` from the active profile in `credentials.json`, or a UUID-valued `TASKAGENT_WORKSPACE`. |

Server SQLite and bootstrap tokens live under **`~/.agents/taskagent/data/`**
(`TASKAGENT_DATA_DIR` when overridden). See [local-dev-data.md](local-dev-data.md).

For IDE chat traceability (`taskagent_session_start` + `metadata`), see
[agent-session-metadata.md](agent-session-metadata.md).

## Remote HTTP MCP

`apps/server` exposes MCP over HTTP at `/v1/mcp`. Cursor can use a remote
entry with URL + headers, so no long-running `taskagent-mcp` stdio process is
needed:

```json
{
  "mcpServers": {
    "taskagent": {
      "url": "http://localhost:8080/v1/mcp",
      "headers": {
        "Authorization": "Bearer ta_svc_..."
      }
    }
  }
}
```

Generate the same shape from the Rust CLI:

```bash
taskagent install --print-config cursor
```

Persist a self-host or local profile into `credentials.json`:

```bash
TASKAGENT_API_URL=http://127.0.0.1:8080 \
TASKAGENT_TOKEN=ta_svc_... \
taskagent install --mode self-host -y
```

For local mode, `TASKAGENT_TOKEN` may be omitted when
`$TASKAGENT_DATA_DIR/bootstrap.token` exists. If `TASKAGENT_DATA_DIR` is unset,
the CLI checks `~/.agents/taskagent/data/bootstrap.token`.

`taskagent-mcp` stdio remains available for clients that do not support remote
HTTP MCP.

## `workspaces.json` shape

```json
{
  "workspaces": {
    "/home/you/projects/taskagent": "019e3052-b262-72a0-8f37-9acac59e83a1",
    "/home/you/projects/taskagent-secondary": "019e5f3f-ab10-7451-8455-6f3807545eb9"
  }
}
```

Bind repo scopes via MCP tool `taskagent_project_use` or override the whole
session with env `TASKAGENT_PROJECT_ID`.

For multi-repo folders, add one entry per repository. Task tools resolve project
scope in this order:

1. explicit `project_id`
2. explicit `project_scope` (or `scope` on tools where `scope` is not already a domain filter)
3. explicit `scope_path`
4. `TASKAGENT_PROJECT_ID`
5. nearest configured repo path prefix for `TASKAGENT_WORKSPACE` / process CWD

Supported launch modes:

- When MCP starts inside one configured repo, unscoped calls use that repo's
  project id.
- When MCP starts in a parent folder that contains multiple configured repos,
  there is no default project. Calls that need a project must pass `project_id`,
  `project_scope`, or `scope_path`; `taskagent_workspace_info` returns every
  known repo scope and the inference error.
- The same `workspaces.json` works in both modes. To bind projects while running
  from a parent folder, call `taskagent_project_use` with `scope_path`
  (relative paths are resolved from `TASKAGENT_WORKSPACE` / process CWD).

## Migration

On first run after upgrading, if `~/.agents/taskagent/workspaces.json` is
missing, the client copies from the first existing legacy file:

1. `~/.config/taskagent/workspaces.json`
2. `$XDG_CONFIG_HOME/taskagent/workspaces.json`
3. `./taskagent/workspaces.json` (old repo-local path — do not commit this)

## Do not use

- `~/.config/taskagent/` — retired in favour of `~/.agents/taskagent/`
- `./taskagent/workspaces.json` in the git tree
- `.local-data/` — unused; ignored if present in a checkout
