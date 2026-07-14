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
fields never touched) until it fits, and a `truncation` marker is attached. A
small fixed slice of the budget is reserved for that appended marker so the
final response (content + marker) still fits `max_tokens * 4` bytes.

Edge case — **protected fields alone exceed a tiny budget**: protected fields
win. We never drop them to hit the number, so the excerpt may exceed the
budget; the `truncation` marker is still emitted so the overflow is explicit
and the caller knows content was withheld. Budget is therefore best-effort:
"as small as possible without violating the no-compress contract".

## Session dedup: `unchanged` / `ref` responses

`daruma_get` and `daruma_plan_get` (`view=detail`) accept an optional
`dedup: true` flag. When set, the server remembers — **for the lifetime of the
MCP session** — which objects it already handed this session in full, and at
which version. A second read of an object that has not changed returns a compact
marker instead of the whole payload:

```json
{ "unchanged": true, "ref": "tsk_1", "since": "2026-01-01T00:00:00Z",
  "id": "tsk_1", "status": "in_progress", "priority": "p0" }
```

- `unchanged: true` — the object is byte-identical to what you were already
  given this session; the full payload is withheld.
- `ref` — **the object id**, human-readable by construction (Daruma ids already
  *are* short handles, e.g. `tsk_1` / `pln_1`; there is no separate ref format
  and no opaque hash). To hydrate the full object, re-read `ref` via the same
  tool **without** `dedup`.
- `since` — the object's `updated_at` at the moment it last changed; "unchanged
  since this timestamp".
- `id` / `status` / `priority` — the protected structural fields (same set as
  the no-compress contract) are still carried so a client can act on lifecycle
  state without hydrating.

### Why it is opt-in

Dedup is **off unless the client passes `dedup: true`**. Existing clients
(Cursor, Claude Desktop, `daruma-claude`) expect a full object from
`daruma_get` / `daruma_plan_get` and would break if they suddenly received an
`unchanged` marker they do not understand. Opt-in is therefore the explicit
"opt-out for clients that don't understand `ref`" the design calls for: everyone
who does not ask keeps today's behaviour byte-for-byte. Progress view
(`daruma_plan_get` default) never dedups — it is already compact.

### How change is detected

The only signal is `updated_at`. Every task/plan mutation bumps it through the
event pipeline, so a matching stamp is a correct-by-construction "nothing
changed" — no payload hashing, no event subscription. The cache lives on the
`ApiClient` (`crates/mcp/src/client.rs`, `dedup_probe`), whose lifetime *is* the
MCP session: one stdio process = one client = one session, so — unlike a
cross-session cache — the server only ever refers you back to content **this
session already saw**. There is no blind-spot where `ref` points at content the
client never received.

On the HTTP MCP transport a fresh `ApiClient` is built per request, so the cache
starts empty and dedup harmlessly degrades to always-full responses; the stdio
transport (how `daruma-claude` runs the server) is where dedup actually saves
tokens.

### Client recipe

When you receive `{"unchanged": true, "ref": <id>, ...}`: you already hold the
full object from earlier this session — reuse it. Only if you truly need the
full body again (e.g. it was evicted from your own context) re-read `ref` with
the same tool and **omit `dedup`** to force a full payload.

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
