# Contributing to Daruma

Thanks for considering a contribution. This document covers the
mechanics: how to file an issue, how to open a pull request, the DCO
sign-off requirement, and the commit-message style. The product
direction and backlog live in the **Daruma** tracker project (plan
**Daruma ROADMAP**, MCP `daruma_plan_list` / web UI); see
[docs/README.md](docs/README.md). The architecture contract is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## License & scope

Daruma is **Apache-2.0 WITH Commons-Clause**
(see [LICENSE](LICENSE) and
[LICENSE.commons-clause.md](LICENSE.commons-clause.md)). In plain
terms:

- Self-host, fork, modify, contribute back — all welcome.
- Sell Daruma as a paid product or managed service — needs a
  commercial licence from the maintainers (see README).

By submitting a contribution you agree to license it under the same
terms; the DCO sign-off below makes that explicit and machine-checkable.

## Developer Certificate of Origin (DCO)

Every commit must be signed off under the
[Developer Certificate of Origin 1.1](https://developercertificate.org/).
The signoff certifies that you wrote the patch (or otherwise have the
right to submit it) and are licensing it under the project licence.

Add `Signed-off-by: Your Name <you@example.com>` to each commit
message — git does this automatically with the `-s` flag:

```bash
git commit -s -m "feat(scope): short summary"
```

To sign off automatically, enable the tracked git hooks once per clone:

```bash
just hooks            # or: git config core.hooksPath .githooks
```

The `prepare-commit-msg` hook in [`.githooks/`](.githooks/) then appends your
`Signed-off-by` trailer to every commit (idempotent — it respects an existing
`git commit -s` and never duplicates the line).

The standard footer looks like:

```
Signed-off-by: Your Name <you@example.com>
```

The name and email must match `git config user.name` /
`git config user.email`. There is **no separate CLA**; the DCO sign-off
is the only legal step required for a contribution to be merged.

A CI check enforces this on every pull request. If the bot complains
about a missing sign-off, rebase the offending commits with
`git rebase --signoff <base>` and force-push the branch.

## Issues

- **Bug reports** — include the version (from `/v1/healthz` if running
  the server, or `cargo pkgid -p daruma-server`), the platform, the
  exact command or request that failed, and the observed vs. expected
  behaviour.
- **Feature requests** — open a discussion before writing code if the
  change touches the event schema, public REST/WS contract, MCP tools,
  or storage migrations. These have wide blast radius; the
  Daruma tracker (catalogue plans §3.7 / §3.8 / MCP Roadmap) lists
  what is open.
- **Security issues** — do not file in the public tracker. Email the
  maintainers (see README) so the fix can ship before disclosure.

## Pull requests

Workflow:

1. Open an issue or a `/plan` document under `.omc/plans/` if the
   change is non-trivial (more than ~100 LOC, touches event schema, or
   crosses crate boundaries). For small fixes, a PR is fine without a
   prior plan.
2. Fork or branch from `main`. Branch naming follows
   `feat/<topic>` / `fix/<topic>` / `docs/<topic>` / `chore/<topic>`.
3. Keep changes surgical. A bug fix is a bug fix; cleanup belongs in a
   separate PR.
4. Run the full local gate before pushing:

   ```bash
   cargo build --workspace
   cargo test  --workspace --exclude daruma-desktop
   cargo clippy --workspace --all-targets -- -D warnings
   cargo fmt --all -- --check
   ```

   `daruma-desktop` is excluded from the workspace test step because
   it pulls GPUI on graphical hosts; CI runs it separately.
5. Open the PR against `main`. Mark it as draft if you want early
   review.
6. Address review comments by adding new commits, not by force-pushing
   over existing ones — the reviewer needs to see the delta. The
   maintainer may squash on merge.

The CI pipeline runs the same four cargo gates plus the DCO check.
Failures should be diagnosed (not bypassed); never skip hooks with
`--no-verify` unless the maintainer asks for it explicitly.

## Commit-message style

We follow a simplified Conventional Commits flavour, scoped by tracker
section (§3.x) where applicable. Examples (from `git log`):

```
feat(§3.4 W2.1): crates/core/src/embed.rs + desktop migrates off internals
feat(§3.9.4): WS Hub DashMap+mpsc fanout (closes §3.9.5) (#21)
fix(docker): switch apt mirror deb.debian.org → http.us.debian.org
docs(§3.4 W1): MODULES.md + MODULE_CONTRACT.md + ARCHITECTURE Core/Modules
chore(deslop): extract common test harness + handler status-transition helper (#18)
```

Header rules:

- `<type>(<scope>): <imperative summary>`, max ~70 chars.
- `<type>`: `feat | fix | docs | chore | refactor | test | perf`.
- `<scope>`: tracker section (`§3.4 W2.1`) when the change implements
  a planned item; otherwise a meaningful subsystem (`docker`,
  `auth`, `mcp`).
- Body: explain *why*, link the closing task id, list anything
  reviewers need to know about migrations or wire compat.
- Trailers: `Closes §… (task <uuid>).`, `Signed-off-by:`, optional
  `Co-Authored-By:`.

## Code style

- Rust 2024 edition, `rustfmt` defaults, `clippy -D warnings`.
- Keep modules under ~300 lines; split rather than nest deeply.
- No god objects; prefer composition over inheritance.
- No business logic in HTTP handlers, WS handlers, or UI views — that
  belongs in `crates/core`. Every mutation goes through
  `CommandBus::dispatch`.
- Avoid unnecessary abstractions and macros unless clearly beneficial.
- All async flows must be explicit; prefer deterministic behavior over magic.
- AI outputs use typed schemas; DB writes only through command handlers.
- Tests: unit tests live next to the code (`#[cfg(test)] mod tests`),
  integration tests for `apps/server` go in `apps/server/tests/`.

## Module changes

If your change adds a new app, crate, or integration:

1. Add a row to [docs/MODULES.md](docs/MODULES.md) under the correct
   kind (`core | transport | client | embed | integration`).
2. Read [docs/MODULE_CONTRACT.md](docs/MODULE_CONTRACT.md) — it lists
   what core promises and what modules must not do.
3. For non-`core` modules, do **not** import from `apps/*` or directly
   from `daruma-storage` / `daruma-events`. Reach for the
   runtime through `daruma_core::embed::*` (embed clients) or the
   HTTP/WS/MCP API (network clients).

## Getting help

- `README.md` — quick start, running the server locally, deploy.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — invariants and crate contracts.
- [docs/architecture-policy.md](docs/architecture-policy.md) — fixed policy decisions.
- [docs/guides/ai-agent.md](docs/guides/ai-agent.md) — AI layer rules and tools.
- [docs/README.md](docs/README.md) — docs layout; backlog in Daruma tracker.

Open a discussion or a draft PR if any of the above is unclear; the
contract bits in particular benefit from being challenged early.
