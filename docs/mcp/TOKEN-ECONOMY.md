# MCP token economy: the no-compress contract

Daruma's MCP tools shrink responses *before* they reach the caller so that a
long-running agent loop does not drown in tokens. Two mechanisms do this:

- **`view=summary` field projection** — list/search/plan_list drop everything
  except a small allowlist of fields (`crates/mcp/src/tools.rs`,
  `summarize_rows` / `summarize_rows_protected`).
- **pagination + truncation markers** — collections are paged
  (`mcp_page_rows`) and single objects can be excerpted to a token budget
  (`bounded_excerpt`); both attach a `truncation` handle describing what was
  withheld.

This document fixes the **contract** those mechanisms must honour.

## What may be compressed, and what may not

There is exactly one class of fields we are allowed to drop or truncate:
**prose** — free-form human text whose omission the caller can recover by
re-reading the full object. Concretely: `description`, comment `body`,
search `snippet`, plan `goal`, document `content`, and similar.

Everything else that a caller needs to *decide what to hydrate next* must
survive intact. These are the **protected fields** — see
`PROTECTED_SUMMARY_FIELDS` in `crates/mcp/src/tools.rs`:

| Field                                          | Why it is protected                          |
| ---------------------------------------------- | -------------------------------------------- |
| `id`                                           | the short handle to re-fetch the object      |
| `status`                                       | lifecycle state — drives "is this actionable"|
| `priority`                                     | ordering / triage signal                     |
| `project_id`, `task_id`, `plan_id`, `parent_plan_id` | FK-style references used to follow relations |
| `error`, `last_error` *(reserved)*             | failure signal must never be silently hidden |

`error`/`last_error` are **reserved**: no `Task`/run/plan projection carries
such a field today, but if one is added it must be exempt from summarisation
from the first commit rather than accidentally compressed away. Reserving the
names now means a future view cannot regress the contract.

### Enforcement

Per-view allowlists (the `&["id", "title", …]` arrays at the list/search/
plan_list call sites) list only the *view-specific* fields. They are passed
through `summarize_rows_protected`, which unions them with
`PROTECTED_SUMMARY_FIELDS`. So even if a new view forgets to name `id` /
`status` / `priority` / a `*_id`, those fields are still emitted. Because
`keep_keys` copies only fields that are actually present on a row, unioning in
a protected field a given row lacks is a no-op — existing view output is
unchanged byte-for-byte.

`title` is **not** prose. It is a short identifying label and is kept by every
summary view; only `description`/`snippet`/`body`/`content`/`goal`-style bulk
text is subject to compression.

## Truncation markers

When a list is paged or a single object is excerpted, the response carries a
`truncation` object (`TruncationMarker`) so the caller can decide whether the
withheld tail is worth a follow-up read *without* a blind `daruma_get` /
`daruma_plan_get`:

- `pointer` — how to hydrate the rest: a pagination cursor for a list, or the
  object id for a single-object excerpt.
- `remaining_bytes` — serialized bytes of the withheld content
  (`serde_json::to_vec(...).len()`, the same measure as the server-side
  `result_bytes` telemetry).
- `remaining_tokens_estimate` — `remaining_bytes / 4`, an approximation (real
  tokenisation varies; treat as an order-of-magnitude hint).
- `summary` — one human-readable line, e.g.
  `"12 more item(s) available, ~3400 bytes (~850 tokens); paginate with the cursor"`.

The existing `has_more` / `truncated` / `next_cursor` / `returned` / `total`
fields are unchanged; `truncation` is additive.

## Bounded excerpt reads

`daruma_get`, `daruma_plan_get` (`view=detail`) and `daruma_doc_get` accept an
optional `max_tokens` budget. Without it, behaviour is unchanged — the full
object is returned. With it, if the serialized object exceeds
`max_tokens * 4` bytes, prose fields are trimmed (longest first, protected
fields never touched) until it fits, and a `truncation` marker is attached.

Edge case — **protected fields alone exceed a tiny budget**: protected fields
win. We never drop them to hit the number, so the excerpt may exceed the
budget; the `truncation` marker is still emitted so the overflow is explicit
and the caller knows content was withheld. Budget is therefore best-effort:
"as small as possible without violating the no-compress contract".

## Design rationale: pre-injection compression preserves the prompt cache

This is **server-side pre-injection** compression: the response is shrunk
*before* it is ever placed into the caller's context window. That is
deliberately different from approaches that rewrite conversation history the
model has already seen.

Prompt caching keys on an exact prefix of the token stream. Once a tool result
has been injected into the transcript, editing it — summarising it after the
fact, dropping old tool outputs, re-ordering messages — changes that prefix
and **invalidates the cache** from the edit point onward, forcing a full
re-encode of everything after it. That is expensive and, over a long agent
loop, recurring.

Compressing at the source avoids this entirely. The bytes that enter the
transcript are already small, so the cached prefix is never mutated: earlier
turns stay verbatim and keep hitting the cache. We pay the token savings once,
at injection time, and never disturb history. That is why Daruma projects
`view=summary` and bounds excerpts in the MCP layer rather than post-hoc
compacting the dialogue.
