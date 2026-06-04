# Agent session metadata (IDE traceability)

When an MCP/IDE agent starts work, it should open a **TaskAgent session** with
`metadata` so tasks can be linked back to a client chat and transcript.

## MCP workflow

1. `taskagent_workspace_info` — note `mcp_agent_id`.
2. `taskagent_session_start` with `metadata` (see schema below).
3. `taskagent_create` (or plan flow) — then `taskagent_comment` on the root task:

   ```text
   session: <session_id from step 2>
   ```

4. On completion: `taskagent_session_end` with the same session id.

## Recommended `metadata` object

| Key | Example | Purpose |
|-----|---------|---------|
| `client` | `cursor` | IDE / runner |
| `model` | `composer-2.5` | Model id or display name |
| `chat_id` | opaque string | Client conversation id |
| `transcript_path` | `/home/.../agent-transcripts/abc.jsonl` | Path to chat log |
| `workspace_path` | `/home/.../projects/taskagent` | Repo root |

Environment defaults (merged when omitted in the call):

- `TASKAGENT_CLIENT`
- `TASKAGENT_MODEL`
- `TASKAGENT_CHAT_ID`
- `TASKAGENT_TRANSCRIPT_PATH`
- `TASKAGENT_WORKSPACE` (or process CWD)

Caller-provided `metadata` fields override env defaults.

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
    "workspace_path": "/home/user/projects/taskagent"
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

TaskAgent does **not** store IDE transcripts; `transcript_path` is an opaque
pointer for humans/tools outside TaskAgent.

## MCP tools

| Tool | Role |
|------|------|
| `taskagent_session_start` | Create session + metadata |
| `taskagent_session_get` | Fetch session by id |
| `taskagent_session_list` | List sessions for `agent_id` |
| `taskagent_session_end` | Close session |
