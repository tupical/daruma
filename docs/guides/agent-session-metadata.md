# Agent session metadata (IDE traceability)

When an MCP/IDE agent starts work, it should open a **Daruma session** with
`metadata` so tasks can be linked back to a client chat and transcript.

## MCP workflow

1. `daruma_workspace_info` — note `mcp_agent_id`.
2. `daruma_session_start` with `metadata` (see schema below).
3. `daruma_plan_materialize` — the only intake path (plan-only intake) — then `daruma_comment` on the root task:

   ```text
   session: <session_id from step 2>
   ```

4. On completion: `daruma_session_end` with the same session id.

## Recommended `metadata` object

| Key | Example | Purpose |
|-----|---------|---------|
| `client` | `cursor` | IDE / runner |
| `model` | `composer-2.5` | Model id or display name |
| `chat_id` | opaque string | Client conversation id |
| `transcript_path` | `/home/.../agent-transcripts/abc.jsonl` | Path to chat log |
| `workspace_path` | `/home/.../projects/daruma` | Repo root |
| `git_work_context` | object below | Git snapshot observed by the client |

Environment defaults (merged when omitted in the call):

- `DARUMA_CLIENT`
- `DARUMA_MODEL`
- `DARUMA_CHAT_ID`
- `DARUMA_TRANSCRIPT_PATH`
- `DARUMA_WORKSPACE` (or process CWD)

Caller-provided `metadata` fields override env defaults.

## Git work context

Local stdio captures missing `metadata.git_work_context` when processing
`daruma_session_start`, using the caller's `workspace_path` or the local
workspace/CWD default. The snapshot is persisted through the existing session
metadata/event projection; no additional session entity is created.

```json
{
  "repo_root": "/home/user/projects/repo",
  "worktree_path": "/home/user/worktrees/feature",
  "head_sha": "0123456789012345678901234567890123456789",
  "branch_ref": "refs/heads/feature",
  "merge_request_id": "42",
  "observed_at": "2026-09-11T00:00:00Z"
}
```

`repo_root` is the first/main working tree reported by Git, while
`worktree_path` identifies the actual checkout. `head_sha` is the full commit
hash and remains the code anchor when branch names change. Detached HEAD has
`branch_ref: null`; an unborn branch has `head_sha: null`. Git unavailable or
a non-repository directory leaves the whole context absent. If the installed
Git cannot enumerate worktrees, `repo_root` is null.

MR/PR identity is explicit: pass it in the snapshot or set
`DARUMA_MERGE_REQUEST_ID` for local stdio. No network lookup or branch-name
guessing occurs. Existing explicit snapshots, including unknown fields, are
preserved. Git status, diffs, remotes and credentials are not collected.

Hosted HTTP MCP never runs Git to infer the caller's checkout. Its caller must
collect this snapshot locally and pass it in `metadata`. The values are
client-reported provenance, not verified authorization or evidence that an MR
was merged. The snapshot describes session start; after switching checkouts
or committing, start a new session to record the new work context.

## HTTP API

```http
POST /v1/sessions
Authorization: Bearer …
Content-Type: application/json

{
  "agent_id": "019e…",
  "metadata": {
    "client": "cursor",
    "model": "composer-2.5",
    "chat_id": "composer-chat-42",
    "transcript_path": "/home/user/.cursor/projects/.../uuid.jsonl",
    "workspace_path": "/home/user/projects/daruma"
  }
}
```

Response `data` is the full `AgentSession` (including `id` and `metadata`).

```http
GET /v1/sessions/{id}
GET /v1/sessions?agent_id={uuid}
```

## Resolving a bare agent UUID on a task

Task fields `created_by` / `updated_by` may show `Actor::Agent { id, name: "mcp" }`.
That `id` is the MCP process agent id — **not** the session id.

To find context:

1. `GET /v1/sessions?agent_id=<uuid>` — list sessions for that agent.
2. Pick the session whose `started_at` matches the task window.
3. Read `metadata.transcript_path` / `metadata.chat_id`.
4. Or search task comments for `session: <session_id>`.

Daruma does **not** store IDE transcripts; `transcript_path` is an opaque
pointer for humans/tools outside Daruma.

## MCP tools

| Tool | Role |
|------|------|
| `daruma_session_start` | Create session + metadata |
| `daruma_session_get` | Fetch session by id |
| `daruma_session_list` | List sessions for `agent_id` |
| `daruma_session_end` | Close session |
