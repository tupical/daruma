# MCP client local data (`daruma mcp`)

The stdio MCP binary keeps **no** state in the repository. All on-disk
files for the client live under a single directory:

```text
~/.agents/daruma/
  credentials.json   # optional local/self-host profiles
  workspaces.json    # legacy scope bindings; imported to the server once,
                     # then renamed to workspaces.json.migrated
  data/              # server SQLite (default DARUMA_DATA_DIR)
    daruma.sqlite
    bootstrap.token
```

## Environment

| Variable | Purpose |
|----------|---------|
| `DARUMA_WORKSPACE` | Workspace key (default: process CWD). Used to infer the nearest repo scope bound on the server (`/v1/repo-scopes`). |
| `DARUMA_PROJECT_ID` | Overrides the server-side scope binding for this session. |
| `DARUMA_AGENT_DIR` | Override agent data root (default `~/.agents/daruma`). |
| `DARUMA_WORKSPACES_FILE` | Override path of the legacy `workspaces.json` imported on startup (see [Migration](#migration)). |
| `DARUMA_API_URL` / `DARUMA_TOKEN` | Remote server (see [ai-agent.md](ai-agent.md)). `daruma mcp --api-url … --token …` overrides both. Without a token, `daruma mcp` reads the active profile from `credentials.json`; an explicit URL still replaces the profile's `server_url`. |
| `DARUMA_WORKSPACE_ID` | Optional workspace UUID sent as `X-Daruma-Workspace-Id`. If unset, uses `workspace_id` from the active profile in `credentials.json`, or a UUID-valued `DARUMA_WORKSPACE`. |

Server SQLite and bootstrap tokens live under **`~/.agents/daruma/data/`**
(`DARUMA_DATA_DIR` when overridden). See [local-dev-data.md](local-dev-data.md).

For IDE chat traceability (`daruma_session_start` + `metadata`), see
[agent-session-metadata.md](agent-session-metadata.md).

## Tool profiles

The advertised tool surface is profile-gated: `default` (compact,
workflow-first, 33 tools) or `full` (the complete catalogue). Select with
`daruma mcp --profile full`, `DARUMA_MCP_PROFILE=full`, or
`/v1/mcp?profile=full`; unset means `default`. Hidden tools are not
callable in `default` — the error names the fix. Composition, migration
notes, and the new-tool checklist live in
[../mcp/PROFILES.md](../mcp/PROFILES.md).

## Token-safe recipes

Use one scoped inventory call first. Do not fan out into archive/history/graph
tools unless the user explicitly asks for that wider context.

| Question | Recipe | Max calls | Forbidden default |
|----------|--------|-----------|-------------------|
| What is open? | `daruma_list` with `status="active"` and `project_id`, `project_scope`, or `scope_path`. Keep the first page small (`limit=10` where supported). | 1 | `status="all"`, completed/archive scans, workspacegraph discovery |
| What is my active work? | `daruma_list` scoped to the project with `status="in_progress"`; filter by owner/client only when the tool/request supports it. | 1-2 | unscoped all-project inventory, session/history scans |
| What is the active plan? | `daruma_plan_list` with active statuses scoped to the project; if one plan must be inspected, call `daruma_plan_get` for that id. | 2 | `plan_list status="completed"`, enumerating completed plans |
| What tasks belong to this branch? | `daruma_search` for `branch:<name>` with `scope="comments"` and project scope. | 1 | using search as general inventory, workspacegraph search first |
| Close completed work. | `daruma_list status="active"` scoped to the project, then mutate only tasks whose completed state is confirmed by local evidence. | 1+N | bulk-closing unverified tasks, `status="all"` cleanup sweeps |

When a response reports more pages or a broader archive, show that fact to the
user and wait for an explicit request before fetching more.

## Remote HTTP MCP

`apps/server` exposes MCP over HTTP at `/v1/mcp`. Cursor can use a remote
entry with URL + headers, so no long-running `daruma mcp` stdio process is
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

`daruma mcp` stdio remains available for clients that do not support remote
HTTP MCP.

## Repo scope bindings

Repo scope bindings (`scope_path → project_id`) are stored on the server in
the `repo_scopes` table, not in a local file. `GET /v1/repo-scopes` returns
every binding; `PUT /v1/repo-scopes` with
`{"scope_path": "...", "project_id": "..."}` upserts one, and
`"project_id": null` removes it:

```json
[
  { "scope_path": "/home/you/projects/daruma", "project_id": "01900000-0000-7000-8000-000000000001" },
  { "scope_path": "/home/you/projects/daruma-secondary", "project_id": "01900000-0000-7000-8000-000000000002" }
]
```

The server stores paths in canonical form (trailing slash trimmed, Windows
spellings such as `C:\Repos\app` and `C:/Repos/app/` collapse to one row).
Because bindings are server-side, local stdio sessions and hosted
(`/v1/mcp`) sessions resolve the same mapping. A server that predates
`/v1/repo-scopes` yields no bindings; explicit `project_id` arguments keep
working there.

Bind repo scopes via MCP tool `daruma_project_use` or override the whole
session with env `DARUMA_PROJECT_ID`. When a `scope_path` has no binding and
the server runs with `DARUMA_AUTO_PROVISION_REPO_PROJECT` on (default off),
the client asks `POST /v1/repo-scopes/provision` to create and bind a project
named after the directory.

For multi-repo folders, add one binding per repository. Task tools resolve project
scope in this order:

1. explicit `project_id`
2. explicit `project_scope` (or `scope` on tools where `scope` is not already a domain filter)
3. explicit `scope_path`
4. `DARUMA_PROJECT_ID`
5. nearest configured repo path prefix for `DARUMA_WORKSPACE` / process CWD

A relative `scope_path` resolves against the local workspace key
(`DARUMA_WORKSPACE`, else the CWD of `daruma mcp`), so it works only in a
stdio session. A hosted (`/v1/mcp`) session has no workspace key: pass an
absolute `scope_path` — POSIX (`/home/you/repo`), Windows (`C:\Repos\app`,
`C:/Repos/app`) or UNC (`\\host\share`). A relative one is rejected with
"relative `scope_path` … needs a local workspace; pass an absolute path", and
`daruma_project_use` without `scope_path` fails with "no local workspace for
this MCP session; pass `scope_path`". Step 5 never applies there either, so an
unscoped hosted call has no default project.

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
  as a server-side repo scope binding for later calls.
- `status` is optional on the list tools: `daruma_list` defaults to `active`,
  `daruma_plan_list` to `draft,active`. There is no `ready`/`blocked` task
  status — ready plan tasks come from `daruma_plan_progress` /
  `daruma_plan_drain_next`, and the 400 for an unknown status lists the valid
  values.
- Id parameter names differ between tools, so the mismatched name is
  accepted as an alias. Every task, plan, run, document, session, rule, work
  unit, artifact and relation tool takes its schema name, the kind's own name
  (`task_id`, `plan_id`, `run_id`, `doc_id` / `document_id`, `session_id`,
  `rule_id`, `work_unit_id`, `artifact_id`, `relation_id`) or the generic `id`:
  `daruma_get {task_id}` and `daruma_comment {id}` both work, as do
  `daruma_plan_progress {id}` and `daruma_run_complete {id}`. The schemas of
  the key tools — `daruma_get`, `daruma_update`, `daruma_set_status`,
  `daruma_complete`, `daruma_comment`, `daruma_relations`, `daruma_claim`,
  `daruma_release`, `daruma_plan_get`, `daruma_plan_progress`,
  `daruma_plan_drain_next`, `daruma_plan_next_task` — list both names and
  require neither, so a client that validates arguments against the schema
  accepts either one; a call with neither passes the schema and fails in the
  tool with the accepted names. "At least one of" is not expressed as a
  top-level `anyOf`: the Anthropic API rejects tool schemas with a top-level
  `anyOf`/`oneOf`/`allOf`, and no tool schema has one
  (`crates/mcp/tests/it/schema_id_alias_contract.rs`). Bulk tools take
  `ids` or `task_ids` (an array, or a single `task_id`). A tool with two ids of
  different kinds (`daruma_plan_add_task`, `daruma_run_start_step`, …) lets a
  generic `id` fill the missing one; `from_work_unit_id` / `to_work_unit_id` of
  `daruma_handoff_request` take no alias. `run_id` is optional on
  `daruma_plan_next_task` — the server generates one. An id whose prefix names
  another kind (`pln_…` where a task is expected) fails before any request, and
  a missing id lists the accepted names. Bare UUIDs carry no kind, so a plan
  UUID sent to a task tool still reaches the server and returns 404. Project,
  agent, workspace, handoff, finding, history and graph-node ids keep their
  single name.
- The same server-side bindings serve both modes. To bind projects while
  running from a parent folder, call `daruma_project_use` with `scope_path`
  (relative paths are resolved from `DARUMA_WORKSPACE` / process CWD).

## Migration

Earlier clients kept bindings in `~/.agents/daruma/workspaces.json`
(`DARUMA_WORKSPACES_FILE` when overridden), shaped
`{"workspaces": {"<repo path>": "<project id>"}}`. On `daruma mcp` (stdio)
startup the client imports that file into the server:

1. If `~/.agents/daruma/workspaces.json` is missing, it is first copied from
   the first existing legacy location:
   1. `~/.config/daruma/workspaces.json`
   2. `$XDG_CONFIG_HOME/daruma/workspaces.json`
   3. `./daruma/workspaces.json` (old repo-local path — do not commit this)

   The legacy source is not deleted: remove it after the import, or it is
   copied and imported again on the next startup.
2. Each non-empty binding is sent to `PUT /v1/repo-scopes`. A binding whose
   project no longer exists (HTTP 404) is dropped.
3. After all bindings are pushed, the file is renamed to
   `workspaces.json.migrated` and is no longer read. Bindings are managed
   only on the server from then on; editing the renamed file has no effect.

If any other `PUT` fails, the file stays in place and the import is retried
on the next startup. Hosted `/v1/mcp` sessions never read a local
`workspaces.json`.

## Do not use

- `~/.config/daruma/` — retired in favour of `~/.agents/daruma/`
- `./daruma/workspaces.json` in the git tree
- `.local-data/` — unused; ignored if present in a checkout
