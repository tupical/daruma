# Comment conventions

Daruma comments support two layers of semantics:

1. **`kind`** — structured classification via `daruma_comment` (`intent`, `progress`, `outcome`, `blocker`, `research`). Stored in the `comments.kind` column.
2. **Body prefixes** — lightweight, zero-migration conventions encoded in the comment `body` text itself. Agents and search tools filter on these prefixes with `LIKE`.

## Body prefix: `lesson:`

**Purpose:** post-mortem / retrospective note — what went wrong, what to do differently next time.

**When to use:**

- After completing a non-trivial task where the approach mattered.
- When a bug fix revealed a systemic gap (missing test, wrong assumption, flaky dependency).
- Before closing an epic — capture durable knowledge for future agents.

**Format:**

```text
lesson: <one-line summary>

<optional details: root cause, fix, prevention>
```

**Examples:**

```text
lesson: always run cargo test -p daruma-mcp after editing tools.rs — dispatch match arms are not compile-checked across crates
```

```text
lesson: SQLite WAL mode required before concurrent plan_next_task claims

Root cause: default journal mode serialized writers.
Fix: PRAGMA journal_mode=WAL in pool init.
```

**MCP usage:**

```json
{
  "task_id": "<uuid>",
  "body": "lesson: verify migration order before deploy",
  "kind": "outcome"
}
```

`kind` is optional. Prefer `outcome` when the lesson closes the task; `research` when documenting an investigation.

**Recall:** `daruma_lesson_recall` (full profile) / `daruma_search` filter `comments.body LIKE 'lesson:%'`. No schema migration — prefix is the contract.

## Body prefix: `flap:`

**Purpose:** a failed executable check that did not reproduce on a rerun of
the unchanged tree — a flap, not a defect ([EXECUTOR-LOOP.md — Failed
checks](../mcp/EXECUTOR-LOOP.md#failed-checks-rerun-before-the-first-edit)).

**When to use:** the task ends with a blocker instead of a passing check, so
the flap has no evidence `flaps[]` entry to live in. With a passing check,
record it in the evidence payload instead.

**Format:** `flap: <signature>` — the first line of the error or the exit
code, no stack trace — optionally followed by free text.

```text
flap: test result: FAILED. 41 passed; 1 failed

Rerun of `cargo test -p daruma-storage claim_ttl` on the unchanged tree passed.
```

**Recall:** `daruma_search` filter `comments.body LIKE 'flap:%'`. No schema
migration — prefix is the contract; no `CommentKind` for it.

## Body prefix: `branch:`

**Purpose:** tie a task to a git branch for branch-scoped work by leaving a
comment on it.

**Format:** `branch: <branch-name>` — one space after the colon — optionally
followed by free text.

```text
branch: feature/mcp-search active work
```

**Recall:** `daruma_search` (REST `GET /v1/search`) with the query
`branch: <prefix>`; the space after the colon is optional in the query. It
returns only comments whose body starts with `branch: <prefix>`, newest first,
each hit carrying its `task_id`; task and plan scopes are skipped for this
query, and `project_id` still narrows it. The match is a prefix match on the
branch name: `branch: feature/` finds every `feature/…` branch, and
`branch: feature/a` also finds `feature/ab`. `%` and `_` in the name are
literal. A bare `branch:` with no name is an ordinary text search.

```text
GET /v1/search?query=branch:%20feature/mcp-search&scope=comments&project_id=<id>
```

finds `branch: feature/mcp-search active work` and skips
`branch: other active work`.

This searches comment bodies only — it does not filter tasks or plans by their
`git_context`. No schema migration — prefix is the contract; no `CommentKind`
for it.

## Related

- [ai-agent.md](ai-agent.md) — AI layer rules and MCP tool overview
- [../mcp/EXECUTOR-LOOP.md](../mcp/EXECUTOR-LOOP.md) — executor loop; use `outcome` comments when finishing steps
