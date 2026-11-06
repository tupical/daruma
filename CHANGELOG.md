# Changelog

This file tracks public, user-visible changes for Daruma releases.

For the current pre-release history in Russian, see [CHANGELOG.ru.md](CHANGELOG.ru.md).

## Unreleased

### Local changes no longer miss the server after a failed write

- `daruma-desktop` committed a command first and queued its events for the
  server afterwards. A crash or a storage error in between left the change on
  the device with nothing to carry it out, and a command of several events
  could queue only some of them: `sync` reported nothing and the server and
  other devices never saw it. An event and its queue entry now commit in one
  transaction, so a failed write takes the whole command back and reports it,
  and a stopped process leaves every logged event queued.
- A command stopped between that commit and its projectors is on its way to the
  server, and the next `sync` rebuilds the device's rows so it shows up there
  too, as it did for an interrupted sync.
- Events go out in the order of the local log, under an origin the replica keeps
  across restarts instead of a new one per process, so a retry keeps its
  identity. The server already takes a retried event once.
- A replica upgraded from an older binary looks for such events once, at its
  next `sync`: anything of the kinds desktop commands make, from the anonymous
  local user, with no origin, no queue entry and no copy in the server log.
  These stay on the device: an incoming event from a server the replica has left
  reads exactly the same, so sending them on its own could publish that server's
  data to this one. Every `sync` lists them and delivers everything else, but
  later changes of the same tasks wait behind them and `sync` counts those too:
  a server that gets a change of a task it has not seen drops it, and the
  create that comes later starts the task over. `sync --restore-lost` sends them
  all in log order, and is for a replica that has only ever synced with this
  server. A change the older binary itself delivered ahead of a lost event is on
  the server already and keeps its place. Events still in the outbox or refused by the server stay
  as they are. The search reads the full server log once on a device that has
  anything to check, and the first sync fails if that read does.

### A sync stopped halfway no longer loses events on the device

- `daruma-desktop sync` logged an incoming event first and ran the projectors
  afterwards, one by one. A crash or a projector error in between left the
  event in the local log without its effect, the retry took the known id for
  a finished event, and the cursor moved past it: a task stayed invisible on
  that device for good, with its history and activity incomplete. An event,
  all its projections, its history and activity rows and the cursor now change
  in one transaction, so a stopped sync leaves nothing behind and the next one
  picks the event up.
- A failure now ends the batch, and sync reports it on every run until it is
  resolved. An event no projector can apply keeps the device from moving on,
  where it used to be skipped on the second try.
- A replica that synced before this change cannot tell which logged events
  took effect, so its next `sync` rebuilds once: tasks, projects, comments and
  task history from the server log up to its cursor plus what only this device
  has, and the activity feed from the local log. This costs one full replay per
  device, and at cursor 0 too if the device holds any events. Pending and
  refused outbox events stay as they are, and events past the cursor arrive
  through the ordinary delta.

### A new device no longer starts from an incomplete snapshot

- A bootstrap snapshot took its `seq` from the event log and its content from
  the live projections, and an event is committed before it is applied. An
  event caught in between, or left unapplied by a projector error or a crash,
  sat under `seq` without being in the snapshot, so the device never replayed
  it and silently missed the task or comment. Snapshots are now folded from
  the event log through the same projectors, so `seq` holds every event up to
  it. Snapshots written before this change are never served again, and until
  the writer produces a new one devices replay the whole log.
- A desktop replica that synced before this change cannot tell whether it was
  bootstrapped from such a snapshot, so its next `sync` rebuilds tasks,
  projects and comments once from the full server log. This costs one full
  replay per device. Events still in the outbox or refused by the server stay
  applied on top, and so does a command another process finishes during the
  rebuild, which swaps the rows in one write transaction: a failed or
  interrupted rebuild changes nothing and reruns on the next `sync`. The
  server's log must be the one the replica came from: a server
  with an empty log is refused rather than adopted.

### Revoking a token ends its open WebSocket and its cached HTTP access

- `DELETE /v1/tokens/{id}` used to change only the token row, so an open
  WebSocket kept running commands and receiving events on the old
  `AuthContext`, and the HTTP cache kept accepting the bearer for up to 30
  seconds. Once the `204` returns, the token's sockets get the `auth_revoked`
  error and a 1008 close, frames queued for them are dropped, and later HTTP
  requests get `401 auth_revoked` even if a verification that started earlier
  fills the cache afterwards. Only that token is affected: other tokens of the
  same agent or device keep working. Revoking a device still refuses all of
  its tokens with `unauthorized`.
- A socket rereads the token row before each client frame runs and before each
  server frame is written, so a revoke from another process or a lost
  notification still holds. A quiet socket finds out at its next `Ping`, within
  25 seconds. This adds one primary-key read per WebSocket frame. If the row
  cannot be read, the socket closes with `auth_storage` and 1011; the client
  can reconnect.
- Limits: a frame that passed this check before the revoke committed still runs
  or is delivered, and a request already past the auth middleware finishes. A
  revoke that skips this server's hub reaches the HTTP cache only when the
  entry expires, 30 seconds after the cache fill rather than after the revoke,
  or sooner if one of the token's sockets finds the revoked row first. Token
  expiry during an open socket is not rechecked.

### Revoking a device closes a WebSocket that was still connecting

- A socket subscribed to device revocations only after authentication, so a
  revoke landing in between left it open on the old `AuthContext`: it kept
  running commands and receiving events. A connection now watches revocations
  before it validates the token and rechecks the device before admission.
  Once a revoke returns, the device's frames are refused on ingress and its
  outgoing queue is replaced by the `unauthorized` error and a 1008 close. A
  broadcast missed through lag no longer hides the revocation.

### Tasks created by a split keep their origin

- After `daruma_split`, each child lost `source_event_id`: the split event set
  it, and the `TaskCreated` that followed cleared it again. `TaskCreated` for a
  split child now carries the id of its `TaskSplitGenerated`, so the link
  survives both the live write and a replay of the stored log, and a nested
  split points at its own event. Children split before this change keep `NULL`;
  there is no backfill.

### The import boundary guards catch ordinary imports

- The `audit-imports` CI workflow used BRE escapes under `grep -E`, so a plain
  `use daruma_desktop::SomeType;` in the core layer or
  `use daruma_storage::Db;` in `apps/desktop/src` passed as `OK`. Both guards
  now live in `scripts/audit-imports.sh` with correct ERE patterns, and CI
  first runs `scripts/audit-imports.test.sh` against fixtures. Imports through
  `daruma_core::embed` stay allowed; `just audit-imports` runs the same checks
  locally.

### `just server` uses the same data directory as the server and CLI

- With `DARUMA_AGENT_DIR` set and `DARUMA_DATA_DIR` unset, `just server` used
  to force `~/.agents/daruma/data`, while `daruma-server` started directly and
  the CLI used `$DARUMA_AGENT_DIR/data` — two databases and two bootstrap
  tokens. `just server` now leaves the choice to the server, so all three
  agree: `DARUMA_DATA_DIR`, else `$DARUMA_AGENT_DIR/data`, else
  `~/.agents/daruma/data`. The local-dev-data guide lists every combination.

### `daruma_complete` keeps the `comment` summary

- `daruma_complete … comment="<summary>"`, as the `daruma-next` prompt and
  the Claude/Codex/Cursor next commands used to suggest, no longer drops the
  summary: it is stored as `note.result_summary` and posted as an `outcome`
  task comment once the completion is accepted. `comment` also takes the
  `{body, kind}` object of `daruma_set_status`; a different `result_summary`
  beside it is rejected before anything is written. The prompts now name
  `result_summary`.

### Superseded artifacts are hidden on read (ADR-0019)

- **Breaking:** `GET /v1/artifacts` and `daruma_artifact_list` without `status`
  return only live artifacts. Pass `include_superseded=true` for the full
  registry; `status=deprecated` still lists deprecated ones.
- Adding a `supersedes` relation (`from` replaces `to`) deprecates `to` in the
  same command with reason `superseded by <from uri>`. A deprecated artifact
  cannot supersede another.
- `POST /v1/artifacts/{id}/deprecate` and `daruma_artifact_deprecate` accept
  `reason`; it is required unless an incoming `supersedes` relation names the
  replacement. `POST /v1/artifacts/{id}/status` to `deprecated` needs that
  relation.

## 0.4.0 — 2026-08-28

Plan-only intake (ADR-0007), lifecycle rules with evidence-gated escapes,
an Artifact Registry, and a month of hardening across MCP, AI infra and
multi-agent coordination. 74 commits since 0.3.0.

### Blocked rules explain how to satisfy them

`rule_blocked` errors now keep the operator-facing message and append a
machine-readable `unblock` JSON suffix for every blocking rule. Each hint names
the required evidence, an actionable scope, and its reuse ceiling; requirements
that cannot be satisfied before entity creation say so instead of suggesting a
not-yet-persisted id.

### Plans retain the brief they came from

Plans can now keep the original brief or prompt that produced them in an
optional, write-once `source_brief`. Set it during intake with
`daruma_plan_materialize` or `POST /v1/plans`; previously the database had
room for it, but no intake path could populate it.

### Evidence no longer reaches further than it can mean anything

One `evidence_submit` at tenant scope used to satisfy a rule for every entity
in the tenant, permanently — a global off-switch with a single audit record.
"This task has acceptance criteria" recorded tenant-wide is a claim about
tasks nobody has written yet: policy wearing the costume of proof.

Each evidence kind now carries a ceiling on how widely it can be read.
`acceptance_criteria_defined`, `completion_note` and `owner_assigned` state a
fact about one entity and are read only at its own scope; `impact_assessment`
and `risk_check_completed` reach the plan; `decision_record` and
`artifact_created` the project; `document_read_ack` stays tenant-wide, because
reading a document once legitimately covers everything below it. A record
above its ceiling is still stored — evidence is an immutable audit fact — but
it satisfies nothing.

Existing records are neither migrated nor deleted — evidence is immutable, and
capping reach at match time makes an over-wide record inert on its own. The
`target` axis is unchanged: broader proof still covers a narrower ask there,
because targets name things that already exist.

### `set_status` can carry an override reason

Overriding a blocking lifecycle rule always required three things: `force`, a
non-empty reason, and a rule that permits override. The reason had no wire
field, so the escape hatch existed in the design and was reachable from
nowhere — `force` alone is refused, by design, because an override that
leaves no trace is indistinguishable from the rule not existing.

`set_status` now accepts `override_reason` (MCP: `daruma_set_status`). Blank
reasons still do not override, and a single non-overridable rule still blocks
the whole set. This is an escape hatch: the normal way past a rule is to
satisfy it with evidence.

### Plan status can override an allowed lifecycle block

`set_plan_status` now carries the same `force` and `override_reason` contract
through the command, MCP tool, and HTTP route. An allowed override returns the
bypassed `PlanBeforeApprove` rule as a warning; blank reasons and any
non-overridable blocking rule still reject the transition. The plan-status
route now also forwards `recommendation` warnings that HTTP/MCP previously
dropped by returning a hard-coded empty warning list.

### `can_start` now answers for lifecycle rules too

`GET /v1/tasks/{id}/can_start` (MCP: `daruma_can_start`) used to consider
only relation blockers. A task held back by a `required` lifecycle rule
came back `ready: true`, and the very next `set_status in_progress`
returned `409 rule_blocked` — the one question the endpoint exists to
answer, answered wrongly.

It now dry-runs the same lifecycle gate the transition passes, and the
response carries two additive fields: `rule_blockers` (rules that block;
non-empty means `ready: false`) and `rule_warnings` (advisory rules,
which never move `ready`). Both are omitted when empty, so a workspace
with no rules sees the response it always had.

### Lazy repo-scope project auto-provisioning (opt-in)

When an MCP call arrives under a repo `scope_path` that has no
`repo_scopes` binding, the server can now lazily create a Daruma project
(title = `basename(scope_path)`) and bind the scope to it, so a fresh
repo becomes tracked on first touch without a manual
`daruma_project_use`. Off by default (OSS/self-host behaviour is
unchanged: an unbound `scope_path` still errors); enable with
`DARUMA_AUTO_PROVISION_REPO_PROJECT=1`. Provisioning is idempotent and
race-safe — concurrent first-touch calls converge on a single project
(`POST /v1/repo-scopes/provision`).

## 0.3.0 — 2026-07-10

### MCP tool-surface profiles

`tools/list` is now profile-gated: the new `default` profile advertises a
compact, workflow-first surface of 31 tools; `full` keeps the complete
catalogue (~94 tools) unchanged. Select with `daruma mcp --profile`,
`DARUMA_MCP_PROFILE`, or `/v1/mcp?profile=`. **Unset now means
`default`** — clients that depend on advanced tools (history, documents,
sessions, workspacegraph, AI ops, bulk ops) must opt into `full`; hidden
tools return an actionable error instead of dispatching. See
[docs/mcp/PROFILES.md](docs/mcp/PROFILES.md) for composition and migration.

- Every advertised tool now carries a human-readable `title` and MCP
  `ToolAnnotations` (`readOnlyHint`, `destructiveHint`, `idempotentHint`,
  `openWorldHint`).
- The internal catalogue is restructured: each tool declares domain,
  profile, and an annotation preset — new tools cannot skip the decision.
- Tool descriptions were tightened for decision-making (shorter, fewer
  cross-tool warnings) without changing names or input schemas.

### task.due webhooks

A due-date watchdog tick (`DARUMA_DUE_TICK_SECS`, default 60 s, `0`
disables) now emits a `task.due` event when an active task's `due_at`
passes — once per (task, deadline) value, deduped across restarts via the
`task_due_notifications` projection (migration 0032). Webhook
subscriptions pick it up by kind like any other event; changing the
deadline re-arms the notification.

### Generic fenced leases (multi-agent coordination, P1)

`work_leases` generalizes from exclusive path globs to mode-aware
resource leases (migration 0033): `mode` (`exclusive` | `shared_read` |
`review` | `intent` — only writes conflict, intent is advisory),
`target_uri` with scheme-dispatched conflict matching (`file://` glob
overlap; `artifact://`/`contract://`/`env://` exact canonical match),
and a monotonic per-resource `fencing_token` issued inside the same
transaction as the grant — stale holders cannot pass
`check_fencing_token` after a re-grant. `reserve_files` (HTTP `/v1/leases`
and the MCP tool) accepts optional `targets` + `mode` and returns leases
carrying tokens; the legacy `paths`-only call is unchanged. The MCP
`daruma_healthz` tool moved into the `default` profile.

### WorkUnit layer (multi-agent coordination, P3)

A task can now decompose into **work units** — the minimal dispatchable
unit for several agents working one task. `POST /v1/work-units` creates
units (with declared `artifact_refs` and acceptance criteria);
`POST /v1/work-units/drain-next` atomically claims the next dispatchable
unit (single-statement CAS — concurrent callers get distinct units) and
acquires its declared exclusive resource leases in the same dispatch,
returning a briefing `{work_unit, leases (fencing tokens), acceptance}`;
a lease conflict reverts the claim. Complete/release endpoints, the
`work_units` projection (migration 0035), WorkUnit* events on the new
WS `WorkUnits` channel, and five MCP tools (full profile) round it out.
Lazy activation: tasks without units are completely untouched.

### Auto-append into Interview / Human Log

Project activity now writes itself into the auto-created documents:
agent activity (agent task ops, runs, run notes) appends to
**Interview**, human milestones (user task ops, plan completion,
project renames) append to **Human Log**. Per-project toggles — ON by
default, also for pre-existing projects — live at
`GET/PATCH /v1/projects/{id}/settings` and the MCP tools
`daruma_project_settings_get` / `_update`; changes are event-sourced
(`ProjectSettingsChanged`, migration 0034) so other clients update in
realtime. See docs/guides/documents-auto-append.md.

### Async AI operation events (WS Channel::AiOps)

Server-side AI operations (`/v1/ai/decompose/{task}`,
`/v1/ai/analyze-complexity/{plan}`) now push typed progress events —
`ai_operation_started` → `ai_operation_phase_changed` (`llm_call`,
`apply`) → `ai_operation_completed` (`ok` / `error: …`) — on the new
WS `AiOps` channel, so clients render progress without polling. The
HTTP responses are unchanged.

### Workspace auto-resolution

`POST /v1/workspace-registry/resolve` maps a filesystem root onto its
logical workspace + default project, creating and binding both on first
contact (longest project-root prefix wins; `create:false` probes only;
`workspace_id` targets an existing workspace). New MCP tools (full
profile): `daruma_workspace_resolve` (persists the resolved project
as the scope default), `daruma_workspace_list`, and
`daruma_project_move_workspace`.

### Scheduler correctness (multi-agent coordination, P2)

The plan task resolver now honors cross-task `Blocks` relations (same
semantics as `can_start`) in every dispatch path — `drain_next`,
`ready_drain`, `next-task`, and `plan_progress.next_ready` — so
concurrent agents can no longer each grab one side of a mutually
blocking pair. Multi-target lease reservation is all-or-none over
canonically sorted targets inside a single immediate transaction,
making opposite-order bulk acquisition deadlock-free.

### AI prompt hardening

Grounding context (task bodies, comments, event payloads) interpolated
into AI prompts is now fenced as explicit untrusted data with embedded
fence-escape neutralization. See docs/guides/ai-agent.md.

## 0.2.0

### One unified `daruma` binary

The stdio MCP server is now a subcommand of the CLI: a single `daruma`
binary is the CLI, the launcher, and the MCP server — one artifact configures
and serves everything.

- `daruma mcp` serves the stdio MCP server, superseding the standalone
  `daruma-mcp` binary. Register it with
  `claude mcp add daruma -- daruma mcp`.
- Bare `daruma` (no subcommand) prints HTTP-MCP connect instructions. With
  credentials it emits a ready-to-paste snippet for whatever server
  `~/.agents/daruma/credentials.json` points at.
- `daruma install --claude` writes the project policy (`CLAUDE.md`) and the
  `.omc` guard, now the single source of truth for that text (shared
  byte-for-byte with the `daruma-claude` plugin).

### Cloud-agnostic core

- The `daruma` binary references no hosted service: it reads a generic
  `server_url` + `token` from credentials and works against any server,
  self-hosted or otherwise.
- Removed the cloud-flavored `install.sh` and its GitHub Pages workflow from
  the OSS repository; cloud onboarding is no longer shipped from OSS.

### Server

- The binary download endpoint moved from
  `/v1/downloads/daruma-mcp/{platform}` to
  `/v1/downloads/daruma/{platform}` and serves the unified binary.

### Client plugins

- `daruma-claude` delegates policy writing to the `daruma` binary when it
  is on `PATH`, falling back to its bundled Node writer otherwise.
- `daruma-cursor` no longer hardcodes a hosted URL in its install hints.

### Docs

- README rewritten as a leaner, overview-first document.

## 0.1.0

Initial OSS release preparation:

- Local-first task runtime with REST, WebSocket, and MCP surfaces.
- Event-sourced task, project, comment, agent inbox, webhook, and auth core.
- Claude, Cursor, and Codex companion packages for agent workflows.
- Public repository cleanup for self-hosted development.
