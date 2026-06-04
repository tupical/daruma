# AI agent layer

The runtime AI is an **autonomous task operator**, not a chat assistant.

## Rules

1. AI **never** writes to storage directly — only `Command` via `CommandBus`.
2. Every AI action produces **events** (traceable, reversible via event log).
3. Prefer **tool-calling** with typed JSON schemas (`crates/ai/src/tools.rs`).
4. Avoid destructive changes; ask for confirmation when uncertain.

## Surfaces

| Surface | Role |
|---------|------|
| `crates/ai` | OpenAI Responses API, prompts (`prompts/*.toml`), parse/decompose/scope/research |
| `apps/server` | `POST /v1/ai/*` HTTP endpoints |
| `taskagent-mcp` | `taskagent_ai_*` tools for external agents |
| MCP agents (Cursor, Claude) | Primary consumers — use MCP tools, not raw SQL |

## HTTP / MCP tools (high level)

- `ai_parse`, `ai_decompose`, `ai_analyze_complexity`, `ai_scope`, `ai_research`
- Task mutations: `taskagent_create`, `taskagent_capture`, `taskagent_capture_batch`, `set_status`, `complete`, `split`, `comment`, plans/runs/claims
- Plan executor: `taskagent_plan_progress`, `taskagent_plan_next_task`, `taskagent_run_*`

Canonical schemas live in code; when the wire format changes, update `crates/ai` and MCP tool descriptors together.

## Further reading

- [ARCHITECTURE.md](../ARCHITECTURE.md) — strict rules §1–§7
- [MODULE_CONTRACT.md](../MODULE_CONTRACT.md) — module boundaries
