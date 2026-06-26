# MCP client local data (`daruma-mcp`)

The stdio MCP binary keeps **no** state in the repository. All on-disk
files for the client live under a single directory:

```text
~/.agents/daruma/
  credentials.json   # optional local/self-host profiles
  workspaces.json    # map: workspace/scope path → default project id
  data/              # server SQLite (default DARUMA_DATA_DIR)
    daruma.sqlite
    bootstrap.token
```

## Environment

| Variable | Purpose |
|----------|---------|
| `DARUMA_WORKSPACE` | Workspace key (default: process CWD). Used to infer the nearest configured repo scope in `workspaces.json`. |
| `DARUMA_PROJECT_ID` | Overrides the on-disk default for this session. |
| `DARUMA_AGENT_DIR` | Override agent data root (default `~/.agents/daruma`). |
| `DARUMA_WORKSPACES_FILE` | Override path to `workspaces.json` only. |
| `DARUMA_API_URL` / `DARUMA_TOKEN` | Remote server (see [ai-agent.md](ai-agent.md)). When unset, `daruma-mcp` reads the active profile from `credentials.json`. |
| `DARUMA_WORKSPACE_ID` | Optional workspace UUID sent as `X-Daruma-Workspace-Id`. If unset, uses `workspace_id` from the active profile in `credentials.json`, or a UUID-valued `DARUMA_WORKSPACE`. |

Server SQLite and bootstrap tokens live under **`~/.agents/daruma/data/`**
(`DARUMA_DATA_DIR` when overridden). See [local-dev-data.md](local-dev-data.md).

For IDE chat traceability (`daruma_session_start` + `metadata`), see
[agent-session-metadata.md](agent-session-metadata.md).

## Tool profiles

The advertised tool surface is profile-gated: `default` (compact,
workflow-first, 31 tools) or `full` (the complete catalogue). Select with
`daruma mcp --profile full`, `DARUMA_MCP_PROFILE=full`, or
`/v1/mcp?profile=full`; unset means `default`. Hidden tools are not
callable in `default` — the error names the fix. Composition, migration
notes, and the new-tool checklist live in
[../mcp/PROFILES.md](../mcp/PROFILES.md).

## Remote HTTP MCP

`apps/server` exposes MCP over HTTP at `/v1/mcp`. Cursor can use a remote
entry with URL + headers, so no long-running `daruma-mcp` stdio process is
needed:

```json
{
  "mcpServers": {
    "daruma": {
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
daruma install --print-config cursor
```

Persist a self-host or local profile into `credentials.json`:

```bash
DARUMA_API_URL=http://127.0.0.1:8080 \
DARUMA_TOKEN=ta_svc_... \
daruma install --mode self-host -y
```

For local mode, `DARUMA_TOKEN` may be omitted when
`$DARUMA_DATA_DIR/bootstrap.token` exists. If `DARUMA_DATA_DIR` is unset,
the CLI checks `~/.agents/daruma/data/bootstrap.token`.

`daruma-mcp` stdio remains available for clients that do not support remote
HTTP MCP.

## `workspaces.json` shape

```json
{
  "workspaces": {
    "/home/you/projects/daruma": "019e3052-b262-72a0-8f37-9acac59e83a1",
    "/home/you/projects/daruma-secondary": "019e5f3f-ab10-7451-8455-6f3807545eb9"
  }
}
```

Bind repo scopes via MCP tool `daruma_project_use` or override the whole
session with env `DARUMA_PROJECT_ID`.

For multi-repo folders, add one entry per repository. Task tools resolve project
scope in this order:

1. explicit `project_id`
2. explicit `project_scope` (or `scope` on tools where `scope` is not already a domain filter)
3. explicit `scope_path`
4. `DARUMA_PROJECT_ID`
5. nearest configured repo path prefix for `DARUMA_WORKSPACE` / process CWD

Supported launch modes:

- When MCP starts inside one configured repo, unscoped calls use that repo's
  project id.
- When MCP starts in a parent folder that contains multiple configured repos,
  there is no default project. Calls that need a project must pass `project_id`,
  `project_scope`, or `scope_path`; `daruma_workspace_info` returns every
  known repo scope and the inference error.
- When `daruma_list` is called without a resolvable project, it does not
  fall back to listing tasks from every project. It returns a compact
  `needs_project_selection` response with project ids/titles/slugs. Ask the
  user which project to use, call `daruma_project_use` with that
  `project_id`, then retry `daruma_list`; the selected project is persisted
  in `workspaces.json` for later calls.
- The same `workspaces.json` works in both modes. To bind projects while running
  from a parent folder, call `daruma_project_use` with `scope_path`
  (relative paths are resolved from `DARUMA_WORKSPACE` / process CWD).

## Migration

On first run after upgrading, if `~/.agents/daruma/workspaces.json` is
missing, the client copies from the first existing legacy file:

1. `~/.config/daruma/workspaces.json`
2. `$XDG_CONFIG_HOME/daruma/workspaces.json`
3. `./daruma/workspaces.json` (old repo-local path — do not commit this)

## Do not use

- `~/.config/daruma/` — retired in favour of `~/.agents/daruma/`
- `./daruma/workspaces.json` in the git tree
- `.local-data/` — unused; ignored if present in a checkout
