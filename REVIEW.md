# REVIEW.md — what matters when reviewing daruma

Only what is specific to this repository. The review procedure (branches,
merge order, the `REVIEW_RESULT` marker, round cap, PRE-EXISTING, the two
operating modes invariant, the real-scenario requirement) lives in the
reviewer prompt `~/.hermes/scripts/mcpbox_review.sh` and is not repeated here.
The architecture contract is [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md);
module boundaries are [docs/MODULE_CONTRACT.md](docs/MODULE_CONTRACT.md).

## What "broken" means here

Collected from real REVIEW REJECTs and filed defects of this project, not
invented. A blocker is a defect this diff introduces from one of these classes,
or an acceptance criterion with no line in the diff.

1. **A consumer across the boundary breaks.** This repo is vendored into
   mcpbox.ru as `vendor/oss`; the cloud reuses `daruma-core`, `daruma-mcp`, the
   `/v1/commands` surface and the literal texts of `crates/mcp/src/server.rs` and
   `crates/mcp/src/tools.rs`. Example: guarding `set_plan_status` on
   `/v1/commands` made the cabinet's "abandon plan" button get 403, while the
   whole suite here was green. Inside this repo the consumers are
   `clients/{claude-plugin,codex-plugin,cursor-plugin,cursor-extension}` (tool
   names, argument names, response shapes), `apps/cli` (stdio MCP) and
   `apps/desktop` (embed via `daruma_core::embed`). Check: `rg` the changed
   command, route, tool or field in `clients/`, `apps/` and, for a changed
   contract, in mcpbox.ru `web/src` and `crates/`.
2. **The wrong principal is accepted.** Claim holder, run owner and evidence
   author must come from the authenticated token, not from the payload. Server
   routes go through `CommandBus::dispatch_authenticated`
   (`crates/core/src/bus.rs`); plain `dispatch`/`dispatch_with_warnings` pass
   no identity, so run-owner checks are skipped. A new route that mutates a
   claim or run through plain `dispatch`, or a command missing from
   `guarded_command_reason` (`apps/server/src/routes/mod.rs`) that should be
   there, is a blocker. Example: acquire held the claim under the token agent
   while stdio MCP released it under its own random `mcp_agent_id` → 403.
3. **Silent data loss or a half write.** `INSERT OR REPLACE` in
   `crates/storage/src/*_repo.rs` on a table with a unique index deletes the
   other row (an `external_key` duplicate inside one `MaterializePlan` batch
   erased the first task and left a dangling `plan_tasks` row). Dedup that only
   queries the database misses duplicates within the same batch: all events of
   a command are built before `append_batch`. A first event persisted and the
   second not, with no retry that restores it, is the same class.
4. **A success that does nothing.** The call returns `ok`, but the state the
   caller wanted did not change: an input field accepted and ignored
   (`daruma_complete` dropping `comment`), or evidence accepted at a scope the
   lifecycle gate never reads, so the rule stays in force. A new field, flag or
   submission path must either take effect or return an error or a
   `MutationWarning` (`crates/api-dto/src/mutation.rs`).
5. **A transition bypasses the single gate.** Lifecycle rules and run-owner
   checks run in one place — `CommandHandler::handle_with_run_owner`
   (`crates/core/src/handler.rs`) — for every status path: `SetStatus`,
   `CompleteTask`, bulk, drain. A new command that changes status, claim or run
   state outside that path, or new events written straight to storage, is a
   blocker.
6. **The agent-facing text lies.** `INSTRUCTIONS` in `crates/mcp/src/server.rs`,
   tool descriptions and schemas in `crates/mcp/src/tools.rs`, `docs/mcp/`,
   `AGENTS.md` and the client `skills/`, `commands/` and `lib/policy.mjs`. Check
   every claim against the code (path:line): tool exists in the advertised
   profile (`ToolProfile`), argument name (`id` versus `task_id`), body key,
   enum values. The cloud test `oss_instructions_mention_pipeline_early_and_keep_headroom`
   (mcpbox.ru `crates/cloud-api/src/v1_gateway.rs`) reads this literal: OSS plus
   platform instructions must stay ≤ 2048 characters with 100 to spare, and the
   word "pipeline" must appear in the first 300 characters.
7. **A migration that breaks old data.** `crates/storage/migrations/` is embedded
   by `sqlx::migrate!` and runs on every existing database. Editing an applied
   file, reusing a number, or a non-null column without a default — blocker.
   Only a new `NNNN_description.sql` with the next number; existing rows must
   survive and read back correctly (0061 rebuilds `audit_findings` because SQLite
   cannot drop NOT NULL, and keeps ids, statuses and operator decisions).
8. **The test proves nothing.** It takes a path the real client does not take
   (a "stdio" test calling `.with_agent_id(...)`, which the real stdio client
   never does), models a state the server never produces (a released claim as
   a task missing from `in_progress`, while real `daruma_release` keeps the
   status and only clears `current_claim`), or its assert is empty by
   construction. Server integration tests belong in `apps/server/tests/`, core
   ones in `crates/core/tests/it/`.

## Not a blocker

- Style, naming, "could be cleaner" — at most **3 remarks** per pass, in one
  comment after the blockers. Drop the rest.
- `cargo fmt` drift in files the diff does not touch. Check formatting only for
  the changed files.
- A defect visible on `origin/main` before this diff — PRE-EXISTING, per the
  reviewer prompt.

## Paths to skip

- `target/`, `data/`, `dist/`, `node_modules/` — build output and local
  databases (gitignored; ignore if they show up untracked).
- `Cargo.lock` — only check that it matches a `Cargo.toml` change in the same
  diff.
- `clients/*/tests/fixtures/` — captured CLI output; read only when the test
  that uses it changed.
- `CHANGELOG*.md`, `docs/VERSION_HISTORY.md` — release notes, not contract.
  `README.ru.md`/`*.ru.md` — check that the Russian side says the same as the
  English one, not the wording.

## Evidence

Every blocker carries `path:line` on the branch under review, a reproduction
(command, call sequence or request) and the criterion it breaks. A guess from a
function or file name is not a blocker: open the code and find the line.
Without `path:line` the remark goes to comments, not blockers.

## Summary

The second line of the verdict (right after `REVIEW ACCEPT`/`REVIEW REJECT`)
starts with a count: `Blockers: N`. With none, say so: `Blockers: 0 — nothing
to fix.` Then the blockers in order, then at most three remarks.
