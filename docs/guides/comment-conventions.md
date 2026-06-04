# Comment conventions

TaskAgent comments support two layers of semantics:

1. **`kind`** — structured classification via `taskagent_comment` (`intent`, `progress`, `outcome`, `blocker`, `research`). Stored in the `comments.kind` column (§3.8.8).
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
lesson: always run cargo test -p taskagent-mcp after editing tools.rs — dispatch match arms are not compile-checked across crates
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

**Recall:** future `taskagent_lesson_recall` / `taskagent_search` tools filter `comments.body LIKE 'lesson:%'`. No schema migration — prefix is the contract.

## Body prefix: `branch:` (planned)

**Purpose:** tie a task or comment to a git branch for branch-scoped work (Plugin P1.5).

**Format:** `branch: <branch-name>` optionally followed by free text.

Example: `branch: feat/plan-progress\nStarted MCP tool + REST endpoint.`

Depends on MCP M3.5 (`taskagent_search` branch filter). Documented here so agents adopt the prefix early.

## Related

- [ai-agent.md](ai-agent.md) — AI layer rules and MCP tool overview
- [../mcp/EXECUTOR-LOOP.md](../mcp/EXECUTOR-LOOP.md) — executor loop; use `outcome` comments when finishing steps
