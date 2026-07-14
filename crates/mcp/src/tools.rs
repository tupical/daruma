//! MCP tool catalogue + dispatch.
//!
//! Every tool is a thin shim over a `daruma-server` HTTP endpoint —
//! the inputs come in as JSON arguments from the MCP client and the
//! outputs are forwarded as JSON `content` text frames.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::client::ApiClient;
use crate::session_metadata;
use crate::workspace;

/// In-memory store of one-time confirm tokens used by `daruma_project_delete`.
///
/// `token → (project_id, issued_at)`.  Tokens expire after [`CONFIRM_TTL`].
/// Cleared on MCP process restart — that is by design: the agent must
/// regenerate the token within the same session.
fn confirm_store() -> &'static Mutex<HashMap<String, (String, Instant)>> {
    static STORE: OnceLock<Mutex<HashMap<String, (String, Instant)>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

const CONFIRM_TTL: Duration = Duration::from_secs(300);
const MCP_DEFAULT_COLLECTION_LIMIT: usize = 10;
const MCP_MAX_COLLECTION_LIMIT: usize = 500;

enum ProjectFilter {
    All,
    Project(String),
    None,
}

fn random_confirm_token() -> String {
    // 128-bit random token, formatted as a UUID.
    uuid::Uuid::new_v4().to_string()
}

fn issue_confirm_token(project_id: &str) -> String {
    let token = random_confirm_token();
    let now = Instant::now();
    let mut guard = confirm_store().lock().expect("confirm_store poisoned");
    // Sweep expired tokens opportunistically to keep the map small.
    guard.retain(|_, (_, ts)| now.duration_since(*ts) < CONFIRM_TTL);
    guard.insert(token.clone(), (project_id.to_string(), now));
    token
}

/// Consume `token`. Returns `Ok(())` if it exists, matches `project_id`, and
/// has not expired; otherwise `Err(reason)`.  The token is removed from the
/// store on every call so a leaked one cannot be replayed.
fn consume_confirm_token(token: &str, project_id: &str) -> std::result::Result<(), &'static str> {
    let mut guard = confirm_store().lock().expect("confirm_store poisoned");
    let (pid, issued_at) = guard.remove(token).ok_or("confirm_token_unknown_or_used")?;
    if Instant::now().duration_since(issued_at) >= CONFIRM_TTL {
        return Err("confirm_token_expired");
    }
    if pid != project_id {
        return Err("confirm_token_project_mismatch");
    }
    Ok(())
}

/// Tool surface profile — which subset of the catalogue is advertised.
///
/// * `Default` — compact, workflow-first surface for everyday agent work.
/// * `Full`    — the complete catalogue (backward-compatible superset).
///
/// Resolution order: explicit override (CLI `--profile`, `/v1/mcp?profile=`)
/// → `DARUMA_MCP_PROFILE` env → built-in `Default`. Clients that need the
/// advanced tools must opt into `full` (see docs/mcp/PROFILES.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolProfile {
    Default,
    Full,
}

impl ToolProfile {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "default" | "core" => Some(Self::Default),
            "full" | "all" | "compat" => Some(Self::Full),
            _ => None,
        }
    }

    /// Resolve from `DARUMA_MCP_PROFILE`; unset or unrecognized → `Default`.
    pub fn from_env() -> Self {
        std::env::var("DARUMA_MCP_PROFILE")
            .ok()
            .and_then(|v| Self::parse(&v))
            .unwrap_or(Self::Default)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Full => "full",
        }
    }
}

/// Internal grouping of tools by workflow domain. Never serialized to MCP
/// clients; exists so catalogue audits and profile composition stay explicit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolDomain {
    Tasks,
    Projects,
    Plans,
    Runs,
    Coordination,
    Sessions,
    Signals,
    Relations,
    WorkspaceGraph,
    Documents,
    History,
    Ai,
    Events,
    Admin,
}

/// Feature tier in the Meisei/Daruma three-tier rubric. Internal catalogue
/// metadata; never serialized to MCP clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Core,
    Enhancing,
    Extending,
}

impl Tier {
    pub fn ru_label(self) -> &'static str {
        match self {
            Self::Core => "основные",
            Self::Enhancing => "усиливающие",
            Self::Extending => "расширяющие",
        }
    }
}

/// MCP `ToolAnnotations` (spec 2025-06-18): behavior hints for clients.
#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    pub title: &'static str,
    pub read_only_hint: bool,
    pub destructive_hint: bool,
    pub idempotent_hint: bool,
    pub open_world_hint: bool,
}

/// Annotation presets. Every tool entry must pick one — there is no
/// `Default` impl on purpose, so a new tool cannot skip the decision.
#[derive(Clone, Copy, Debug)]
enum Ann {
    /// Pure read: no state change, repeatable.
    Read,
    /// Mutating, additive (create/append), not idempotent.
    Write,
    /// Mutating but idempotent (set-style updates, releases).
    WriteIdem,
    /// Deletes/archives/overwrites user-visible data.
    Destructive,
    /// LLM-backed, talks to an external model, persists results.
    AiWrite,
}

impl Ann {
    fn build(self, title: &'static str) -> ToolAnnotations {
        let (read_only, destructive, idempotent, open_world) = match self {
            Ann::Read => (true, false, true, false),
            Ann::Write => (false, false, false, false),
            Ann::WriteIdem => (false, false, true, false),
            Ann::Destructive => (false, true, false, false),
            Ann::AiWrite => (false, false, false, true),
        };
        ToolAnnotations {
            title,
            read_only_hint: read_only,
            destructive_hint: destructive,
            idempotent_hint: idempotent,
            open_world_hint: open_world,
        }
    }
}

/// Static description of a tool, returned by `tools/list`.
///
/// `domain`, `profile`, and `tier` are internal catalogue metadata (skipped in
/// serialization); everything else maps 1:1 onto the MCP `Tool` object.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    pub annotations: ToolAnnotations,
    #[serde(skip)]
    pub domain: ToolDomain,
    #[serde(skip)]
    pub profile: ToolProfile,
    #[serde(skip)]
    pub tier: Tier,
}

#[allow(clippy::too_many_arguments)]
fn tool(
    name: &'static str,
    title: &'static str,
    description: &'static str,
    input_schema: Value,
    domain: ToolDomain,
    profile: ToolProfile,
    tier: Tier,
    ann: Ann,
) -> ToolDefinition {
    ToolDefinition {
        name,
        title,
        description,
        input_schema,
        annotations: ann.build(title),
        domain,
        profile,
        tier,
    }
}

/// Full catalogue of tools (the `full` profile). Use
/// [`tool_definitions_for`] to get a profile-filtered surface.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    use ToolDomain as Dom;
    const D: ToolProfile = ToolProfile::Default;
    const F: ToolProfile = ToolProfile::Full;
    const C: Tier = Tier::Core;
    const E: Tier = Tier::Enhancing;
    const X: Tier = Tier::Extending;

    vec![
        // ── Tasks ─────────────────────────────────────────────────────────
        tool(
            "daruma_create",
            "Create task",
            "Create a new task. `title` is required; everything else is optional. daruma is the single source of truth for tasks/plans — do not also persist them in markdown, TODO files, or .omc/plans/.",
            schema_create(),
            Dom::Tasks, D, C, Ann::Write,
        ),
        tool(
            "daruma_capture",
            "Capture inbox task",
            "Quick-capture a fleeting idea as an inbox task (priority p3). Uses the resolved repo project when unambiguous; pass `project_id`, `project_scope`, or `scope_path` in multi-repo parent folders. Pass `project_id: null` for a project-less inbox task.",
            schema_capture(),
            Dom::Tasks, D, X, Ann::Write,
        ),
        tool(
            "daruma_capture_batch",
            "Capture multiple inbox tasks",
            "Capture multiple inbox tasks in one call. Each string becomes a separate task (priority p3).",
            schema_capture_batch(),
            Dom::Tasks, F, X, Ann::Write,
        ),
        tool(
            "daruma_get",
            "Get task",
            "Fetch a single task by id. Use only when you need fields a recent list/search row does not already carry (those rows include title, status, and priority).",
            schema_with_id("id"),
            Dom::Tasks, D, C, Ann::Read,
        ),
        tool(
            "daruma_update",
            "Update task",
            "Update a task's title, description, or due date. Recorded in the task event/activity log.",
            schema_update(),
            Dom::Tasks, D, C, Ann::WriteIdem,
        ),
        tool(
            "daruma_list",
            "List tasks",
            "List tasks — the default tool for \"what's open / inventory\". Required `status`: a single value (`inbox`/`todo`/`in_progress`/`in_review`/`done`/`cancelled`), a comma-separated list, `active` (all non-terminal), or `all`. Avoid `status=all` unless the user explicitly asked for the archive — it can return a very large response. Optional `project_id` (`inbox` = no project, `all` = every project); when omitted, the resolved repo project is used if unambiguous, otherwise a compact project-selection response is returned.",
            schema_list(),
            Dom::Tasks, D, C, Ann::Read,
        ),
        tool(
            "daruma_search",
            "Search tasks and comments",
            "Full-text lookup across tasks, comments, and plans for a named keyword. Use when the user names concrete text to find; to enumerate open work use `daruma_list status=active` instead. Defaults to a small MCP page and marks truncation.",
            schema_search(),
            Dom::Tasks, D, X, Ann::Read,
        ),
        tool(
            "daruma_lesson_recall",
            "Recall lessons",
            "[Sensemaking layer / deprecated in core] Recall lesson comments. Searches comments whose body starts with `lesson:`; optional `query` narrows the lesson prefix. Lesson recall is a knowledge concern owned by the Sensemaking layer (`satori::lesson_recall`); the core comment store stays, but this tool is out of the default execution profile and reachable only under `full`.",
            schema_lesson_recall(),
            Dom::Tasks, F, X, Ann::Read,
        ),
        tool(
            "daruma_set_status",
            "Set task status",
            "Set a task's status (inbox / todo / in_progress / in_review / done / cancelled).",
            schema_set_status(),
            Dom::Tasks, D, C, Ann::WriteIdem,
        ),
        tool(
            "daruma_set_priority",
            "Set task priority",
            "Set a task's priority (p0 / p1 / p2 / p3).",
            schema_set_priority(),
            Dom::Tasks, D, E, Ann::WriteIdem,
        ),
        tool(
            "daruma_move_project",
            "Move task to another project",
            "Move a task to another project while preserving its id, comments, relations, and event history. Pass `project_id`, `project_scope`, or `scope_path`.",
            schema_move_project(),
            Dom::Tasks, F, C, Ann::WriteIdem,
        ),
        tool(
            "daruma_complete",
            "Complete task",
            "Mark a task as completed. Optionally attach a completion note (reason / result_summary / acceptance_criteria_status / related_artifacts); the completing actor (user vs agent) is recorded automatically.",
            schema_complete(),
            Dom::Tasks, D, C, Ann::WriteIdem,
        ),
        tool(
            "daruma_reopen",
            "Reopen task",
            "Reopen a completed task (sets status back to `todo`).",
            schema_with_id("id"),
            Dom::Tasks, D, E, Ann::WriteIdem,
        ),
        tool(
            "daruma_delete",
            "Delete task",
            "Delete a task permanently.",
            schema_with_id("id"),
            Dom::Tasks, F, C, Ann::Destructive,
        ),
        tool(
            "daruma_split",
            "Split task into subtasks",
            "Split a parent task into 2+ subtasks.",
            schema_split(),
            Dom::Tasks, F, E, Ann::Write,
        ),
        tool(
            "daruma_bulk_set_status",
            "Bulk set task status",
            "Atomically set the same status on up to 50 tasks. Duplicate ids are deduped; fail-fast if any id is missing.",
            schema_bulk_set_status(),
            Dom::Tasks, F, E, Ann::WriteIdem,
        ),
        tool(
            "daruma_comment",
            "Comment on task",
            "Add a comment to a task. Optional semantic `kind` (intent/progress/outcome/blocker/research).",
            schema_comment(),
            Dom::Tasks, D, E, Ann::Write,
        ),
        tool(
            "daruma_can_start",
            "Check task readiness",
            "Check whether a task is ready to start, returning active blockers with title and status.",
            schema_can_start(),
            Dom::Tasks, D, X, Ann::Read,
        ),
        // ── Projects / workspace ──────────────────────────────────────────
        tool(
            "daruma_project_list",
            "List projects",
            "List every project (id, title, description).",
            empty_schema(),
            Dom::Projects, D, C, Ann::Read,
        ),
        tool(
            "daruma_project_create",
            "Create project",
            "Create a new project.",
            schema_project_create(),
            Dom::Projects, F, C, Ann::Write,
        ),
        tool(
            "daruma_project_use",
            "Bind workspace to project",
            "Bind a workspace/repo scope to a daruma project. When MCP runs in a folder containing multiple repos, pass `scope_path` so unscoped parent-folder calls remain explicit. Pass `project_id: null` to clear the selected scope.",
            schema_project_use(),
            Dom::Projects, D, C, Ann::WriteIdem,
        ),
        tool(
            "daruma_project_delete",
            "Delete project",
            "Delete a project. Two-step, destructive: (1) call with only `id` to receive a one-time `confirm_token` (TTL 5 min) plus a contents summary; (2) call again with the same `id`, the issued `confirm_token`, AND `confirm` set to the project's exact title. The server still refuses unless the project has 0 tasks and 0 plans.",
            schema_project_delete(),
            Dom::Projects, F, E, Ann::Destructive,
        ),
        tool(
            "daruma_workspace_info",
            "Show workspace info",
            "Show this MCP session's workspace key, inferred project, inference error, and known repo scopes.",
            empty_schema(),
            Dom::Admin, D, X, Ann::Read,
        ),
        tool(
            "daruma_workspace_resolve",
            "Resolve/bind workspace for a path",
            "Resolve a filesystem root to its logical workspace + default project via the server registry. Unknown roots are created-and-bound on first call (pass `create:false` to probe only); the resolved project is persisted as this scope's default. Use when starting in a repo daruma has never seen.",
            schema_workspace_resolve(),
            Dom::Projects, F, X, Ann::WriteIdem,
        ),
        tool(
            "daruma_workspace_list",
            "List logical workspaces",
            "List logical workspaces from the server registry: id, name, bound filesystem roots, and project count.",
            empty_schema(),
            Dom::Projects, F, X, Ann::Read,
        ),
        tool(
            "daruma_project_move_workspace",
            "Move project to workspace",
            "Move a project into another logical workspace (registry API), optionally binding a filesystem root to the project.",
            schema_project_move_workspace(),
            Dom::Projects, F, E, Ann::WriteIdem,
        ),
        tool(
            "daruma_project_settings_get",
            "Get project settings",
            "Read per-project settings: the auto-append toggles for the Interview (AI log) and Human Log documents (both ON by default).",
            schema_with_id("project_id"),
            Dom::Projects, F, E, Ann::Read,
        ),
        tool(
            "daruma_project_settings_update",
            "Update project settings",
            "Partially update per-project settings: pass `interview` and/or `human_log` booleans to toggle auto-append into the corresponding log document.",
            schema_project_settings_update(),
            Dom::Projects, F, E, Ann::WriteIdem,
        ),
        // ── Lifecycle rules (docs/LIFECYCLE_RULES_SPEC.md) ──────────────────
        tool(
            "daruma_rule_list",
            "List lifecycle rules",
            "List lifecycle rules at a scope. No scope params = tenant (workspace) rules; pass `project_id`, `plan_id`, or `task_id` for a narrower scope. Rules declare what evidence a lifecycle transition requires (read a doc, check impact, attach a completion note). At transition time the gate already reports the active requirement in `rule_warnings`/`rule_blocked`, so this admin tool is for managing rules, not for the hot path.",
            schema_rule_list(),
            Dom::Admin, F, E, Ann::Read,
        ),
        tool(
            "daruma_rule_get",
            "Get a lifecycle rule",
            "Fetch a single lifecycle rule by id.",
            schema_with_id("id"),
            Dom::Admin, F, E, Ann::Read,
        ),
        tool(
            "daruma_rule_create",
            "Create a lifecycle rule",
            "Create a lifecycle rule (admin). A rule is `event → condition → requirement → allowed|warning|blocked`; it has no actions. Pass the `rule` object (rule_key, title, scope, trigger, requirement, mode, message, override_allowed). `mode: required` blocks the transition until satisfied; `recommendation` warns; `off` is inert.",
            schema_rule_create(),
            Dom::Admin, F, E, Ann::Write,
        ),
        tool(
            "daruma_rule_update",
            "Update a lifecycle rule",
            "Patch a lifecycle rule (admin): any of mode, condition, requirement, message, override_allowed, enabled, title. `scope`, `trigger` and `rule_key` are immutable.",
            schema_rule_update(),
            Dom::Admin, F, E, Ann::WriteIdem,
        ),
        tool(
            "daruma_rule_disable",
            "Disable a lifecycle rule",
            "Disable a lifecycle rule (admin). A disabled rule is not evaluated by the gate.",
            schema_with_id("id"),
            Dom::Admin, F, E, Ann::Destructive,
        ),
        // ── Evidence registry (OSS task 019eb65a-3185) ─────────────────────
        tool(
            "daruma_evidence_submit",
            "Record lifecycle evidence",
            "Record evidence that a lifecycle requirement is satisfied, so a `required` rule unblocks the transition. Pass the `evidence` object: `kind` (document_read_ack | impact_assessment | decision_record | completion_note | artifact_created | owner_assigned | acceptance_criteria_defined | risk_check_completed), `scope` (tenant/project/plan/task — same shape as a rule scope), an optional `target` (the doc/module the requirement names; omit to satisfy any target), plus optional bindings (project_id/plan_id/task_id/run_id/artifact_id/rule_id), `reason`, `payload`, and `doc_version` (for document_read_ack). Evidence is immutable; set `supersedes` to replace an earlier record.",
            schema_evidence_submit(),
            Dom::Admin, F, E, Ann::Write,
        ),
        tool(
            "daruma_evidence_list",
            "List lifecycle evidence",
            "List evidence recorded at a scope (tenant by default; pass `project_id`, `plan_id`, or `task_id` for a narrower scope). Superseded records are hidden unless `include_superseded` is true.",
            schema_evidence_list(),
            Dom::Admin, F, E, Ann::Read,
        ),
        // ── Audit primitives ───────────────────────────────────────────────
        tool(
            "daruma_audit_findings",
            "List audit findings",
            "List audit findings for a project (problems a server-side check raised: stale docs, stuck tasks, missing owners, …). Filter by `severity` (error|warn|info), `category`, or `status` (open|acknowledged|muted|resolved). Newest activity first.",
            schema_audit_findings(),
            Dom::Coordination, F, E, Ann::Read,
        ),
        tool(
            "daruma_audit_finding_ack",
            "Acknowledge/mute/resolve a finding",
            "Set the status of an audit finding (operator action): `open`, `acknowledged`, `muted`, or `resolved`. Use `acknowledged` to mark it seen, `muted` to silence it, `resolved` to close it.",
            schema_audit_finding_ack(),
            Dom::Coordination, F, E, Ann::WriteIdem,
        ),
        tool(
            "daruma_audit_stuck_tasks",
            "Tasks stuck in a status",
            "Tasks stuck in a status longer than a threshold (heuristic, no LLM). `status` defaults to `in_progress`; `threshold_hours` defaults to 72. Complements daruma_doctor — this catches tasks wedged in *any* status, not just claim-less in_progress.",
            schema_audit_stuck_tasks(),
            Dom::Coordination, F, E, Ann::Read,
        ),
        tool(
            "daruma_audit_duplicate_tasks",
            "Duplicate-task candidates",
            "Lexical duplicate-task candidates within a project (heuristic, no LLM): title-similar task pairs to review. Not semantic duplicates — a cheap pre-filter for a human pass.",
            schema_audit_duplicate_tasks(),
            Dom::Coordination, F, E, Ann::Read,
        ),
        tool(
            "daruma_audit_unread_documents",
            "Documents not read recently",
            "Documents in a project not read in the last N `days` (default 30); documents never read always qualify. Built on passive read-tracking, distinct from the explicit evidence document_read_ack.",
            schema_audit_unread_documents(),
            Dom::Coordination, F, E, Ann::Read,
        ),
        // ── AI tools ──────────────────────────────────────────────────────
        tool(
            "daruma_ai_analyze_complexity",
            "AI: analyze plan complexity",
            "[Planning layer / deprecated in core] Estimate decomposition complexity for every task in a plan in one batch LLM call. Upserts the `task_complexity_hints` projection (per-task score 1-10, recommended_subtasks, expansion_hint, reasoning). The analysis itself is planning-layer logic (`yatagarasu::analyze_complexity_batch`); this tool remains a delegation-shim until the cloud cutover. Decomposition also lives in the planning layer — there is no core decompose tool to chain into.",
            schema_ai_analyze_complexity(),
            Dom::Ai, F, X, Ann::AiWrite,
        ),
        // ── Events / health ───────────────────────────────────────────────
        tool(
            "daruma_inbox_pull",
            "Pull agent inbox",
            "Poll a single agent's inbox; optionally long-poll up to 60 s.",
            schema_inbox_pull(),
            Dom::Coordination, F, E, Ann::Write,
        ),
        tool(
            "daruma_subscribe_project",
            "Snapshot project events",
            "One-shot snapshot of events for a project (the streaming form lives on /v1/ws).",
            schema_subscribe_project(),
            Dom::Events, F, E, Ann::Read,
        ),
        tool(
            "daruma_events_since",
            "Load events since seq",
            "Load events with `seq > since`, capped at `limit` (default 100).",
            schema_events_since(),
            Dom::Events, F, E, Ann::Read,
        ),
        tool(
            "daruma_healthz",
            "Server health check",
            "Server health check — no auth required.",
            empty_schema(),
            Dom::Admin, D, X, Ann::Read,
        ),
        // ── Plans ─────────────────────────────────────────────────────────
        tool(
            "daruma_plan_create",
            "Create plan",
            "Create a new execution plan for a project. daruma is the single source of truth for tasks/plans — do not also persist them in markdown, TODO files, or .omc/plans/.",
            schema_plan_create(),
            Dom::Plans, D, C, Ann::Write,
        ),
        tool(
            "daruma_plan_update",
            "Update plan",
            "Update a plan's title, description, goal, success criteria, or parent. Pass null for parent_plan_id to unparent (move to root); omit the field to leave the parent unchanged.",
            schema_plan_update(),
            Dom::Plans, F, C, Ann::WriteIdem,
        ),
        tool(
            "daruma_plan_get",
            "Get plan",
            "Fetch a plan by id, including progress metrics — the cheap way to summarize one plan's status (prefer this over enumerating completed plans or tasks).",
            schema_plan_get(),
            Dom::Plans, D, C, Ann::Read,
        ),
        tool(
            "daruma_plan_list",
            "List plans",
            "List plans. Required `status`: `draft`/`active`/`completed`/`abandoned`, a comma-separated list, or `all`. Prefer `draft,active`; completed plans carry their full goal + success criteria and are token-heavy — summarize a single plan with `daruma_plan_get` instead of enumerating. `project_id` uses the resolved repo project when unambiguous; pass `all` to query across projects.",
            schema_plan_list(),
            Dom::Plans, D, C, Ann::Read,
        ),
        tool(
            "daruma_plan_add_task",
            "Attach task to plan",
            "Attach a task to a plan at an optional position with optional dependencies.",
            schema_plan_add_task(),
            Dom::Plans, D, C, Ann::Write,
        ),
        tool(
            "daruma_plan_remove_task",
            "Detach task from plan",
            "Detach a task from a plan. Aborts any in-progress step atomically.",
            schema_plan_task_ref(),
            Dom::Plans, F, C, Ann::Write,
        ),
        tool(
            "daruma_plan_reorder",
            "Reorder plan tasks",
            "Replace the full task order within a plan.",
            schema_plan_reorder(),
            Dom::Plans, F, C, Ann::WriteIdem,
        ),
        tool(
            "daruma_plan_archive",
            "Archive plan",
            "Archive a plan and atomically abort all active runs.",
            schema_with_id("id"),
            Dom::Plans, F, E, Ann::Destructive,
        ),
        tool(
            "daruma_plan_set_status",
            "Set plan status",
            "Transition a plan into a different lifecycle state (draft, active, completed, abandoned). Emits PlanStatusChanged.",
            schema_plan_set_status(),
            Dom::Plans, D, E, Ann::WriteIdem,
        ),
        tool(
            "daruma_plan_next_task",
            "Peek next eligible plan task",
            "Return the first eligible task in a plan for a given run, respecting dependencies. May acquire a claim when `claim_ttl_secs` is set — prefer `daruma_plan_drain_next` for parallel agents.",
            schema_plan_next_task(),
            Dom::Plans, F, E, Ann::Write,
        ),
        tool(
            "daruma_plan_progress",
            "Plan progress snapshot",
            "Executor snapshot for a plan: task counts by status plus the next ready task id (when the plan is Active).",
            schema_with_id("plan_id"),
            Dom::Plans, D, E, Ann::Read,
        ),
        tool(
            "daruma_plan_drain_next",
            "Claim next plan task",
            "Atomically resolve the next eligible plan task and acquire an exclusive claim for this session's agent. Concurrent callers each get a distinct task; returns null when no unclaimed ready task remains. Re-call in a loop to drain a plan across many agents.",
            schema_plan_drain_next(),
            Dom::Plans, D, E, Ann::Write,
        ),
        tool(
            "daruma_plan_graph",
            "Read plan DAG",
            "Read a plan's execution DAG: task nodes plus depends_on and blocks edges.",
            schema_with_plan_id(),
            Dom::Plans, F, X, Ann::Read,
        ),
        tool(
            "daruma_plan_fanout",
            "Plan execution waves",
            "Return parallel execution waves for a plan, respecting depends_on and active Blocks relations.",
            schema_with_plan_id(),
            Dom::Plans, F, X, Ann::Read,
        ),
        tool(
            "daruma_bulk_attach_to_plan",
            "Bulk attach tasks to plan",
            "Atomically attach up to 50 tasks to a single plan. Already-attached tasks are skipped (idempotent); fail-fast if any task or the plan is missing.",
            schema_bulk_attach_to_plan(),
            Dom::Plans, F, E, Ann::WriteIdem,
        ),
        // ── Artifact Registry (P4) ───────────────────────────────────────
        tool(
            "daruma_artifact_register",
            "Register artifact",
            "Register a named artifact URI in the registry (creates a Pending artifact node). \
             `uri` must use a supported scheme: `artifact://`, `file://`, `contract://`, or `env://`. \
             Optional `task_id` links the artifact to the task that produces it.",
            schema_artifact_register(),
            Dom::WorkspaceGraph, F, X, Ann::Write,
        ),
        tool(
            "daruma_artifact_list",
            "List artifacts",
            "List artifacts scoped to a project, task, or both. Returns id, uri, title, status, \
             owner, version. Use to answer \"who owns this\" before writing.",
            schema_artifact_list(),
            Dom::WorkspaceGraph, F, X, Ann::Read,
        ),
        tool(
            "daruma_artifact_impact",
            "Artifact impact analysis",
            "Downstream dependents of an artifact node via ArtDependsOn, ArtImplements, \
             ArtTests, ArtDocuments, ArtSupersedes, ArtConflictsWith, and Produces edges. \
             Answers \"what breaks if I change this artifact\".",
            schema_artifact_impact(),
            Dom::WorkspaceGraph, F, X, Ann::Read,
        ),
        // ── WorkspaceGraph ────────────────────────────────────────────────
        tool(
            "daruma_workspacegraph_status",
            "WorkspaceGraph index health",
            "WorkspaceGraph index health: schema version, node/edge counts, event lag, and last error.",
            empty_schema(),
            Dom::WorkspaceGraph, F, X, Ann::Read,
        ),
        tool(
            "daruma_workspacegraph_context",
            "Graph node neighborhood",
            "Immediate neighborhood of a graph node (incoming/outgoing edges plus ranked neighbors).",
            schema_workspacegraph_context(),
            Dom::WorkspaceGraph, F, X, Ann::Read,
        ),
        tool(
            "daruma_workspacegraph_related",
            "Graph related nodes",
            "Breadth-first related nodes around a graph node, capped by depth and limit.",
            schema_workspacegraph_related(),
            Dom::WorkspaceGraph, F, X, Ann::Read,
        ),
        tool(
            "daruma_workspacegraph_search",
            "Search WorkspaceGraph nodes",
            "[Sensemaking layer / deprecated in core] Full-text search over WorkspaceGraph nodes — for finding a node whose graph neighborhood you then explore. Semantic search is a knowledge concern owned by the Sensemaking layer (`satori::semantic_search`); structural navigation (status/context/related) stays in core. Out of the default execution profile; reachable only under `full`. Not for listing open work (use `daruma_list status=active`).",
            schema_workspacegraph_search(),
            Dom::WorkspaceGraph, F, X, Ann::Read,
        ),
        tool(
            "daruma_workspacegraph_impact",
            "Graph impact analysis",
            "[Sensemaking layer / deprecated in core] Downstream tasks and plans affected through Blocks, PlanContains, and ownership edges. Behavioral impact analysis is a knowledge concern owned by the Sensemaking layer (`satori::impact`); structural navigation (status/context/related) stays in core. Out of the default execution profile; reachable only under `full`.",
            schema_workspacegraph_impact(),
            Dom::WorkspaceGraph, F, X, Ann::Read,
        ),
        // ── Runs ──────────────────────────────────────────────────────────
        tool(
            "daruma_run_start",
            "Start run",
            "Start a new agent run of a plan.",
            schema_run_start(),
            Dom::Runs, D, E, Ann::Write,
        ),
        tool(
            "daruma_run_start_step",
            "Start run step",
            "Mark the beginning of a task step within a run.",
            schema_run_step(),
            Dom::Runs, F, E, Ann::Write,
        ),
        tool(
            "daruma_run_finish_step",
            "Finish run step",
            "Mark the completion of a task step with an outcome.",
            schema_run_finish_step(),
            Dom::Runs, F, E, Ann::Write,
        ),
        tool(
            "daruma_run_complete",
            "Complete run",
            "Terminate a run successfully.",
            schema_with_id("run_id"),
            Dom::Runs, D, E, Ann::WriteIdem,
        ),
        tool(
            "daruma_run_abort",
            "Abort run",
            "Abort a run with a reason (e.g. plan archived or explicit stop).",
            schema_run_abort(),
            Dom::Runs, D, E, Ann::WriteIdem,
        ),
        tool(
            "daruma_run_note_append",
            "Append run note",
            "Append a free-form journal note to an active run. The actor is taken from the MCP session token; body is required (≤ 4 KiB).",
            schema_run_note_append(),
            Dom::Runs, D, E, Ann::Write,
        ),
        tool(
            "daruma_run_log",
            "Append run log entry",
            "Append a leveled progress log entry to an active run. Uses the run notes stream with body formatted as `[level] message`.",
            schema_run_log(),
            Dom::Runs, F, E, Ann::Write,
        ),
        tool(
            "daruma_run_notes_list",
            "List run notes",
            "List journal notes for a run in chronological order. Optional `limit` (default 50, max 500) and `after` (cursor = id of last note from previous page).",
            schema_run_notes_list(),
            Dom::Runs, F, E, Ann::Read,
        ),
        // ── Claims & leases (parallel-agent coordination) ─────────────────
        tool(
            "daruma_claim",
            "Claim task",
            "Acquire an optimistic claim on a task for a given TTL in seconds.",
            schema_claim(),
            Dom::Coordination, D, E, Ann::Write,
        ),
        tool(
            "daruma_release",
            "Release task claim",
            "Release a previously-acquired task claim.",
            schema_release(),
            Dom::Coordination, D, E, Ann::WriteIdem,
        ),
        tool(
            "daruma_reserve_files",
            "Reserve file paths",
            "Reserve resources for a task so parallel agents don't collide. Pass repo-relative `paths` (globs) and/or `targets` URIs (file://, artifact://, contract://, env://) plus an optional `mode` (exclusive default; shared_read/review coexist; intent is advisory). Returns `reserved:true` with leases carrying `fencing_token`, or `reserved:false` with `conflict_path` + `holder` — then take a different task. Re-reserving extends the TTL; leases auto-release when the task closes or the TTL lapses.",
            schema_reserve_files(),
            Dom::Coordination, F, E, Ann::Write,
        ),
        tool(
            "daruma_release_files",
            "Release file leases",
            "Release all file/path leases held by an agent for a task. Usually automatic on task completion; call explicitly to free files early.",
            schema_release_files(),
            Dom::Coordination, F, E, Ann::WriteIdem,
        ),
        tool(
            "daruma_active_work",
            "List active file leases",
            "List the active work backlog: live file/path leases (who is touching which files) for a project. Use before reserving to see contended areas. Pass `project_id` to scope; omit for all.",
            schema_active_work(),
            Dom::Coordination, F, E, Ann::Read,
        ),
        tool(
            "daruma_ready",
            "List project ready pool",
            "List the project-wide ready pool: tasks across ALL active plans whose dependencies are satisfied and that no other agent holds. The read-only view behind `daruma_ready_drain`.",
            schema_ready(),
            Dom::Coordination, F, E, Ann::Read,
        ),
        tool(
            "daruma_ready_drain",
            "Claim next ready task (project-wide)",
            "Atomically claim the next ready task across the project's active plans. Concurrent callers each get a distinct task; sets it in_progress. Returns null when nothing is ready — loop until null.",
            schema_ready_drain(),
            Dom::Coordination, F, E, Ann::Write,
        ),
        tool(
            "daruma_doctor",
            "Reconcile stuck parallel work",
            "Reconcile parallel-agent state for a project: reports tasks stuck `in_progress` with no live claim (an agent likely crashed and its claim TTL lapsed). These are reclaimable — reopen or re-drain them.",
            schema_doctor(),
            Dom::Coordination, F, E, Ann::Read,
        ),
        tool(
            "daruma_suggest_files",
            "Suggest paths to reserve",
            "Suggest path globs to reserve for a task by extracting path-like tokens from its title/description. Use to seed `daruma_reserve_files` at claim time. Heuristic only — review before reserving.",
            schema_suggest_files(),
            Dom::Coordination, F, X, Ann::Read,
        ),
        tool(
            "daruma_work_unit_create",
            "Create work unit",
            "Create a work unit under a task — the minimal dispatchable unit for multi-agent work on one task. Declare `artifact_refs` (file://, artifact://, contract://, env://) so the dispatcher can lease them on claim. Simple tasks don't need work units.",
            schema_work_unit_create(),
            Dom::Coordination, F, E, Ann::Write,
        ),
        tool(
            "daruma_work_unit_list",
            "List task work units",
            "List all work units under a task (full decomposition state, including done/cancelled).",
            schema_with_id("task_id"),
            Dom::Coordination, F, E, Ann::Read,
        ),
        tool(
            "daruma_work_unit_drain_next",
            "Claim next work unit",
            "Atomically claim the next dispatchable work unit under a task and acquire its declared exclusive resource leases. Concurrent callers each get a distinct unit. Returns a briefing {work_unit, leases (with fencing_token), acceptance}; null when nothing is dispatchable; lease_conflict (claim reverted) when the unit's resources are held elsewhere.",
            schema_work_unit_drain(),
            Dom::Coordination, F, E, Ann::Write,
        ),
        tool(
            "daruma_work_unit_complete",
            "Complete work unit",
            "Mark a work unit done with an outcome and the produced artifact URIs (mineable payload). Releases the holder claim.",
            schema_work_unit_complete(),
            Dom::Coordination, F, E, Ann::WriteIdem,
        ),
        tool(
            "daruma_work_unit_release",
            "Release work unit claim",
            "Release a claimed work unit back to the dispatch pool (status returns to ready).",
            schema_with_id("id"),
            Dom::Coordination, F, E, Ann::WriteIdem,
        ),
        tool(
            "daruma_handoff_request",
            "Request work-unit handoff",
            "Request a handoff between two work units (P5): name the artifacts and checklist the consuming unit needs. The consumer is NOT dispatchable until the handoff is accepted. Re-requesting the same (from, to) pair after a rejection reopens the contract.",
            schema_handoff_request(),
            Dom::Coordination, F, E, Ann::Write,
        ),
        tool(
            "daruma_handoff_respond",
            "Accept or reject handoff",
            "Respond to an open handoff contract: `decision: accept` (optional notes) unblocks the consuming unit; `decision: reject` (reason + required_changes) sends it back for a re-request.",
            schema_handoff_respond(),
            Dom::Coordination, F, E, Ann::Write,
        ),
        tool(
            "daruma_handoff_list",
            "List work-unit handoffs",
            "Every handoff contract touching a work unit (either side), newest first — why a unit is (not) dispatchable, without digging through comments.",
            schema_with_id("work_unit_id"),
            Dom::Coordination, F, E, Ann::Read,
        ),
        // ── Sessions ──────────────────────────────────────────────────────
        tool(
            "daruma_session_start",
            "Start agent session",
            "Start a new agent session. Pass `metadata` with client/model/chat_id/transcript_path so work can be traced back to the IDE chat. `agent_id` defaults to this MCP process id.",
            schema_session_start(),
            Dom::Sessions, F, X, Ann::Write,
        ),
        tool(
            "daruma_session_get",
            "Get agent session",
            "Fetch an agent session by id (includes metadata: client, model, chat_id, transcript_path).",
            schema_with_id("id"),
            Dom::Sessions, F, X, Ann::Read,
        ),
        tool(
            "daruma_session_list",
            "List agent sessions",
            "List agent sessions for an agent id (defaults to this MCP process).",
            schema_session_list(),
            Dom::Sessions, F, X, Ann::Read,
        ),
        tool(
            "daruma_session_end",
            "End agent session",
            "End an agent session.",
            schema_with_id("id"),
            Dom::Sessions, F, X, Ann::WriteIdem,
        ),
        tool(
            "daruma_session_set_plan",
            "Set session plan steps",
            "Replace the session's plan-steps list (max 100 steps).",
            schema_session_set_plan(),
            Dom::Sessions, F, X, Ann::WriteIdem,
        ),
        tool(
            "daruma_session_artifact",
            "Attach session artifact",
            "Attach a file/url/diff artifact reference to an agent session.",
            schema_session_artifact(),
            Dom::Sessions, F, X, Ann::Write,
        ),
        tool(
            "daruma_session_artifacts_list",
            "List session artifacts",
            "List artifact references attached to an agent session.",
            schema_with_id("id"),
            Dom::Sessions, F, X, Ann::Read,
        ),
        // ── Signals ───────────────────────────────────────────────────────
        tool(
            "daruma_signal_send",
            "Send run signal",
            "Send a typed signal to a run (stop / elicit / auth_required).",
            schema_signal_send(),
            Dom::Signals, F, E, Ann::Write,
        ),
        tool(
            "daruma_signal_respond",
            "Respond to run signal",
            "Human responds to an elicitation request on a run.",
            schema_signal_respond(),
            Dom::Signals, F, E, Ann::Write,
        ),
        // ── Relations ─────────────────────────────────────────────────────
        tool(
            "daruma_link",
            "Link tasks",
            "Create a typed relation (blocks / relates_to / duplicates) between two tasks. Idempotent via `client_command_id`.",
            schema_link(),
            Dom::Relations, D, E, Ann::WriteIdem,
        ),
        tool(
            "daruma_unlink",
            "Delete task relation",
            "Delete a relation by its id.",
            schema_unlink(),
            Dom::Relations, F, E, Ann::Destructive,
        ),
        tool(
            "daruma_relations",
            "Read task relations",
            "Read 5-group relations projection for a task (blocks, blocked_by, relates_to, duplicates, duplicated_by).",
            schema_relations(),
            Dom::Relations, D, E, Ann::Read,
        ),
        // ── Documents ─────────────────────────────────────────────────────
        tool(
            "daruma_doc_create",
            "Create document",
            "Create a markdown document for a project. `kind` is `interview` or `human_log`; multiple docs of the same kind are allowed.",
            schema_doc_create(),
            Dom::Documents, F, X, Ann::Write,
        ),
        tool(
            "daruma_doc_get",
            "Get document",
            "Fetch a document by id, including its full markdown body.",
            schema_with_id("document_id"),
            Dom::Documents, F, X, Ann::Read,
        ),
        tool(
            "daruma_doc_append",
            "Append to document",
            "Append markdown to a document. A blank-line separator is inserted by the server when the existing body is non-empty.",
            schema_doc_append(),
            Dom::Documents, F, X, Ann::Write,
        ),
        tool(
            "daruma_doc_replace",
            "Replace document body",
            "Replace a document's entire markdown body.",
            schema_doc_replace(),
            Dom::Documents, F, X, Ann::WriteIdem,
        ),
        tool(
            "daruma_doc_rename",
            "Rename document",
            "Rename a document (title only; body is unchanged).",
            schema_doc_rename(),
            Dom::Documents, F, X, Ann::WriteIdem,
        ),
        tool(
            "daruma_doc_archive",
            "Archive document",
            "Soft-archive a document. It remains queryable via `daruma_doc_list` when `include_archived=true`.",
            schema_with_id("document_id"),
            Dom::Documents, F, X, Ann::Destructive,
        ),
        tool(
            "daruma_doc_set_status",
            "Set document status",
            "Change a document's lifecycle status (draft/active/outdated/archived). Setting `archived` behaves like archive; leaving `archived` un-archives.",
            schema_doc_set_status(),
            Dom::Documents, F, X, Ann::WriteIdem,
        ),
        tool(
            "daruma_doc_link_task",
            "Link document to task",
            "Bind a document to a task as its artifact (vision: documents are task artifacts, not free-floating notes). Pass `task_id: null` (or omit it) to unlink back to a project-level document.",
            schema_doc_link_task(),
            Dom::Documents, F, X, Ann::WriteIdem,
        ),
        tool(
            "daruma_doc_list",
            "List documents",
            "List documents for a project. `project_id` uses the resolved repo project when unambiguous; multi-repo parent folders require `project_id`, `project_scope`, or `scope_path`. Optional `kind` filter; archived docs are hidden unless `include_archived=true`.",
            schema_doc_list(),
            Dom::Documents, F, X, Ann::Read,
        ),
        // ── Version history ───────────────────────────────────────────────
        tool(
            "daruma_history_list",
            "List version history",
            "List immutable version records for one task or document, newest first.",
            schema_history_entity(),
            Dom::History, F, X, Ann::Read,
        ),
        tool(
            "daruma_history_get",
            "Get version record",
            "Fetch one immutable version record by version id.",
            schema_with_id("version_id"),
            Dom::History, F, X, Ann::Read,
        ),
        tool(
            "daruma_history_compare",
            "Compare versions",
            "Compare two version numbers for the same task or document.",
            schema_history_compare(),
            Dom::History, F, X, Ann::Read,
        ),
        tool(
            "daruma_history_latest",
            "List latest versions",
            "List latest task/document version records visible to this token.",
            schema_history_latest(),
            Dom::History, F, X, Ann::Read,
        ),
        tool(
            "daruma_history_summary",
            "Version summary timeline",
            "Return a compact agent-readable summary timeline for one task or document.",
            schema_history_entity(),
            Dom::History, F, X, Ann::Read,
        ),
        tool(
            "daruma_history_rollback",
            "Rollback to version",
            "Restore a task or document to a selected immutable version by creating a new rollback version.",
            schema_with_id("version_id"),
            Dom::History, F, X, Ann::Destructive,
        ),
    ]
}

/// Catalogue filtered to `profile`. `Full` returns everything; `Default`
/// returns the compact workflow-first surface.
pub fn tool_definitions_for(profile: ToolProfile) -> Vec<ToolDefinition> {
    tool_definitions()
        .into_iter()
        .filter(|t| profile == ToolProfile::Full || t.profile == ToolProfile::Default)
        .collect()
}

/// True when `name` is a known catalogue tool that the given profile hides.
/// Unknown names return false so dispatch can produce its normal
/// unknown-tool error.
pub fn tool_hidden_in_profile(name: &str, profile: ToolProfile) -> bool {
    profile == ToolProfile::Default
        && tool_definitions()
            .iter()
            .any(|t| t.name == name && t.profile == ToolProfile::Full)
}

/// Profile-gated dispatch: hidden tools are not callable and return an
/// actionable error instead of silently working.
pub async fn call_tool_in_profile(
    client: &ApiClient,
    profile: ToolProfile,
    name: &str,
    arguments: Value,
) -> anyhow::Result<Value> {
    if tool_hidden_in_profile(name, profile) {
        anyhow::bail!(
            "tool `{name}` is not available in the `{}` MCP profile; \
             restart the MCP server with DARUMA_MCP_PROFILE=full \
             (or `daruma mcp --profile full`) to enable the full catalogue",
            profile.as_str()
        );
    }
    call_tool(client, name, arguments).await
}

/// Dispatch a single tool call by name. The MCP client passes `arguments`
/// as a JSON object; this function returns the JSON body the server
/// should embed in `content[0].text`.
pub async fn call_tool(client: &ApiClient, name: &str, arguments: Value) -> anyhow::Result<Value> {
    let args = arguments.as_object().cloned().unwrap_or_default();

    match name {
        "daruma_create" => {
            let mut task = args.get("task").cloned().unwrap_or_else(|| json!({}));
            // Inject the workspace default project if the task didn't
            // specify one explicitly. Use `"project_id": null` in the
            // arguments to opt out and create an inbox-only task.
            if let Some(t) = task.as_object_mut() {
                if !t.contains_key("project_id") {
                    match resolve_project_filter(client, &args, false, true, true).await? {
                        ProjectFilter::Project(pid) => {
                            t.insert("project_id".to_string(), Value::String(pid));
                        }
                        ProjectFilter::None => {}
                        ProjectFilter::All => unreachable!("allow_all=false"),
                    }
                }
                // Normalize explicit nulls to absent.
                if let Some(v) = t.get("project_id") {
                    if v.is_null() {
                        t.remove("project_id");
                    }
                }
            }
            client
                .post_command(json!({"type":"create_task","task": task}))
                .await
        }
        "daruma_capture" => {
            let text = required_string(&args, "text")?;
            create_captured_task(client, &text, &args).await
        }
        "daruma_capture_batch" => {
            let texts = args
                .get("texts")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow::anyhow!("`texts` (array of strings) is required"))?;
            if texts.is_empty() {
                return Err(anyhow::anyhow!("`texts` must contain at least one item"));
            }
            let mut tasks = Vec::with_capacity(texts.len());
            for item in texts {
                let text = item
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("each entry in `texts` must be a string"))?;
                let resp = create_captured_task(client, text, &args).await?;
                tasks.push(resp);
            }
            Ok(json!({ "count": tasks.len(), "tasks": tasks }))
        }
        "daruma_get" => {
            let id = required_string(&args, "id")?;
            client.get_json(&format!("/v1/tasks/{id}")).await
        }
        "daruma_update" => {
            let id = required_string(&args, "id")?;
            let mut patch = Map::new();
            if let Some(title) = args.get("title") {
                let title = title
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("`title` must be a string"))?;
                patch.insert("title".to_string(), Value::String(title.to_string()));
            }
            if let Some(description) = args.get("description") {
                let description = description
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("`description` must be a string"))?;
                patch.insert(
                    "description".to_string(),
                    Value::String(description.to_string()),
                );
            }
            if let Some(due_at) = args.get("due_at") {
                if due_at.is_null() {
                    patch.insert("due_at".to_string(), Value::Null);
                } else {
                    let due_at = due_at
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("`due_at` must be a string or null"))?;
                    patch.insert("due_at".to_string(), Value::String(due_at.to_string()));
                }
            }
            if patch.is_empty() {
                anyhow::bail!("at least one of `title`, `description`, or `due_at` is required");
            }
            client
                .post_command(json!({"type":"update_task","id": id, "patch": patch}))
                .await
        }
        "daruma_list" => {
            let status = required_string(&args, "status")?;
            let view = view_arg(&args, "summary", &["summary", "detail"])?;
            let cursor = args.get("cursor").and_then(|v| v.as_str());
            let limit = mcp_collection_limit(&args);
            let mut params: Vec<(&str, String)> = vec![("status", urlencode(status.trim()))];
            match resolve_project_filter(client, &args, true, false, true).await? {
                ProjectFilter::All => {}
                ProjectFilter::None => {
                    return project_selection_response(client, status.trim()).await;
                }
                ProjectFilter::Project(pid) => params.push(("project_id", urlencode(&pid))),
            }
            let qs = params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            let resp = mcp_page_by_id(
                client.get_json(&format!("/v1/tasks?{qs}")).await?,
                cursor,
                limit,
            );
            Ok(if view == "detail" {
                resp
            } else {
                summarize_rows_protected(
                    resp,
                    &[
                        "id",
                        "title",
                        "status",
                        "priority",
                        "project_id",
                        "updated_at",
                    ],
                )
            })
        }
        "daruma_search" => {
            let query = required_string(&args, "query")?;
            let scope = args.get("scope").and_then(|v| v.as_str());
            let view = view_arg(&args, "summary", &["summary", "detail"])?;
            let cursor = args.get("cursor").and_then(|v| v.as_str());
            let limit = mcp_collection_limit(&args);
            let mut params: Vec<(&str, String)> = vec![("query", urlencode(&query))];
            if let Some(s) = scope {
                let s = s.trim();
                if !s.is_empty() {
                    params.push(("scope", urlencode(s)));
                }
            }
            match resolve_project_filter(client, &args, true, false, false).await? {
                ProjectFilter::All => params.push(("project_id", "all".to_string())),
                ProjectFilter::Project(pid) => params.push(("project_id", urlencode(&pid))),
                ProjectFilter::None => {}
            }
            let qs = params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            let resp = mcp_page_by_offset(
                client.get_json(&format!("/v1/search?{qs}")).await?,
                cursor,
                limit,
            );
            Ok(if view == "detail" {
                resp
            } else {
                summarize_rows_protected(
                    resp,
                    &[
                        "kind",
                        "id",
                        "title",
                        "snippet",
                        "task_id",
                        "plan_id",
                        "project_id",
                    ],
                )
            })
        }
        "daruma_lesson_recall" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_u64());
            let lesson_query = if query.trim().is_empty() {
                "lesson:".to_string()
            } else {
                format!("lesson: {}", query.trim())
            };
            let mut params: Vec<(&str, String)> = vec![
                ("query", urlencode(&lesson_query)),
                ("scope", "comments".to_string()),
            ];
            match resolve_project_filter(client, &args, true, false, false).await? {
                ProjectFilter::All => params.push(("project_id", "all".to_string())),
                ProjectFilter::Project(pid) => params.push(("project_id", urlencode(&pid))),
                ProjectFilter::None => {}
            }
            if let Some(limit) = limit {
                params.push(("limit", limit.to_string()));
            }
            let qs = params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            client.get_json(&format!("/v1/search?{qs}")).await
        }
        "daruma_project_list" => client.get_json("/v1/projects").await,
        "daruma_project_create" => {
            let title = required_string(&args, "title")?;
            let description = args
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let resp = client
                .post_command(
                    json!({"type":"create_project","title": title, "description": description}),
                )
                .await?;
            // Server returns MutationResponse; surface the new project_id
            // up-front for convenience.
            let project_id = resp["data"].as_array().and_then(|arr| {
                arr.iter().find_map(|env| {
                    env.pointer("/payload/project/id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
            });
            Ok(json!({ "project_id": project_id, "events": resp }))
        }
        "daruma_project_delete" => {
            let id = required_string(&args, "id")?;
            let confirm_token = args.get("confirm_token").and_then(|v| v.as_str());
            let confirm = args.get("confirm").and_then(|v| v.as_str());

            // Look up the project so we can match `confirm` against its title
            // and surface a summary regardless of which call this is.
            let projects = client.get_json("/v1/projects").await?;
            let project = projects
                .as_array()
                .and_then(|arr| {
                    arr.iter()
                        .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
                })
                .ok_or_else(|| anyhow::anyhow!("project_not_found: {id}"))?
                .clone();
            let title = project
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Counts for the summary; we hit the server fresh so the agent
            // sees the same state the route handler will gate on.
            let tasks = client
                .get_json(&format!(
                    "/v1/tasks?project_id={}&status=all",
                    urlencode(&id)
                ))
                .await?;
            let tasks_count = tasks.as_array().map(|a| a.len()).unwrap_or(0);
            let plans = client
                .get_json(&format!(
                    "/v1/plans?project_id={}&status=all",
                    urlencode(&id)
                ))
                .await?;
            let plans_count = plans.as_array().map(|a| a.len()).unwrap_or(0);

            // ── Step 1: no token yet → issue one and return preview. ─────
            if confirm_token.is_none() && confirm.is_none() {
                let token = issue_confirm_token(&id);
                return Ok(json!({
                    "status": "confirmation_required",
                    "project_id": id,
                    "project_title": title,
                    "tasks_count": tasks_count,
                    "plans_count": plans_count,
                    "empty": tasks_count == 0 && plans_count == 0,
                    "confirm_token": token,
                    "ttl_seconds": CONFIRM_TTL.as_secs(),
                    "instructions": "To delete, call again with the same `id`, this `confirm_token`, and `confirm` set to the project's exact title.",
                }));
            }

            // ── Step 2: both fields must be present, no partial calls. ───
            let token = confirm_token
                .ok_or_else(|| anyhow::anyhow!("`confirm_token` is required on the second call"))?;
            let confirm = confirm.ok_or_else(|| {
                anyhow::anyhow!("`confirm` is required and must equal the project title")
            })?;
            if confirm != title {
                anyhow::bail!(
                    "confirm_mismatch: expected exact title `{}`, got `{}`",
                    title,
                    confirm
                );
            }
            consume_confirm_token(token, &id).map_err(|reason| anyhow::anyhow!(reason))?;

            // Final guard before the wire call.  The server enforces this
            // again — we surface it early for a more useful error message.
            if tasks_count > 0 || plans_count > 0 {
                anyhow::bail!("project_not_empty: tasks={tasks_count}, plans={plans_count}");
            }

            let resp = client
                .delete_json(&format!("/v1/projects/{}", urlencode(&id)))
                .await?;
            Ok(json!({
                "status": "deleted",
                "project_id": id,
                "project_title": title,
                "response": resp,
            }))
        }
        "daruma_project_use" => {
            let view = workspace::ScopeView::fetch_or_empty(client).await;
            let scope_path = args.get("scope_path").and_then(|v| v.as_str());
            match args.get("project_id") {
                Some(v) if v.is_null() => {
                    let scope = view.scope_for_binding(scope_path)?;
                    workspace::bind(client, &scope, None).await?;
                    Ok(json!({"workspace": view.key(), "scope": scope, "project_id": Value::Null}))
                }
                Some(v) => {
                    let pid = v
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("`project_id` must be a string or null"))?;
                    let scope = view.scope_for_binding(scope_path)?;
                    workspace::bind(client, &scope, Some(pid)).await?;
                    Ok(json!({"workspace": view.key(), "scope": scope, "project_id": pid}))
                }
                None => anyhow::bail!("`project_id` is required (use null to clear)"),
            }
        }
        "daruma_workspace_info" => {
            let view = workspace::ScopeView::fetch_or_empty(client).await;
            let (inferred_project, inferred_project_error) = match view.inferred_project() {
                Ok(project_id) => (project_id, None),
                Err(err) => (None, Some(err.to_string())),
            };
            Ok(json!({
                "workspace": view.key(),
                "mcp_agent_id": client.agent_id(),
                "default_project": inferred_project.clone(),
                "inferred_project": inferred_project,
                "inferred_project_error": inferred_project_error,
                "scopes": view.scopes()
                    .iter()
                    .map(|(scope, project_id)| json!({
                        "scope": scope,
                        "name": std::path::Path::new(scope)
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or(scope),
                        "project_id": project_id,
                    }))
                    .collect::<Vec<_>>(),
            }))
        }
        "daruma_set_status" => {
            let id = required_string(&args, "id")?;
            let status = required_string(&args, "status")?;
            let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
            client
                .post_command(
                    json!({"type":"set_status","id": id, "status": status, "force": force}),
                )
                .await
        }
        "daruma_set_priority" => {
            let id = required_string(&args, "id")?;
            let priority = required_string(&args, "priority")?;
            client
                .post_command(json!({"type":"set_priority","id": id, "priority": priority}))
                .await
        }
        "daruma_move_project" => {
            let id = required_string(&args, "id")?;
            let project_id = match resolve_project_filter(client, &args, false, false, true).await? {
                ProjectFilter::Project(pid) => pid,
                ProjectFilter::None => {
                    anyhow::bail!("`project_id`, `project_scope`, or `scope_path` is required")
                }
                ProjectFilter::All => unreachable!("allow_all=false"),
            };
            client
                .post_command(json!({
                    "type":"update_task",
                    "id": id,
                    "patch": {
                        "project_id": project_id
                    }
                }))
                .await
        }
        "daruma_complete" => {
            let id = required_string(&args, "id")?;
            // Assemble an optional completion note from the loose args. Only
            // include `note` when at least one field is present, so a bare
            // `{id}` call stays byte-identical to the legacy command.
            let mut note = serde_json::Map::new();
            for key in ["reason", "result_summary", "acceptance_criteria_status"] {
                if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
                    note.insert(key.to_string(), json!(v));
                }
            }
            if let Some(arts) = args.get("related_artifacts").and_then(|v| v.as_array()) {
                if !arts.is_empty() {
                    note.insert("related_artifacts".to_string(), json!(arts));
                }
            }
            let mut cmd = json!({"type":"complete_task","id": id});
            if !note.is_empty() {
                cmd["note"] = Value::Object(note);
            }
            client.post_command(cmd).await
        }
        "daruma_delete" => {
            let id = required_string(&args, "id")?;
            client
                .post_command(json!({"type":"delete_task","id": id}))
                .await
        }
        "daruma_split" => {
            let parent = required_string(&args, "parent")?;
            let subtasks = args
                .get("subtasks")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("`subtasks` (array) is required"))?;
            client
                .post_command(json!({"type":"split_task","parent": parent, "subtasks": subtasks}))
                .await
        }
        "daruma_bulk_set_status" => {
            let ids = args
                .get("ids")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("`ids` (array of task ids) is required"))?;
            let status = required_string(&args, "status")?;
            client
                .post_command(json!({"type":"bulk_set_status","ids": ids, "status": status}))
                .await
        }
        "daruma_bulk_attach_to_plan" => {
            let plan_id = required_string(&args, "plan_id")?;
            let task_ids = args
                .get("ids")
                .or_else(|| args.get("task_ids"))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("`ids` (array of task ids) is required"))?;
            client
                .post_command(json!({
                    "type":"bulk_attach_to_plan",
                    "plan_id": plan_id,
                    "task_ids": task_ids,
                }))
                .await
        }
        // Deprecated delegation-shim: this is a thin HTTP forward to the
        // `/v1/ai/analyze-complexity` route, whose complexity-analysis logic
        // now canonically lives in the planning layer
        // (`yatagarasu::analyze_complexity_batch`). Kept until the cloud
        // cutover rewires the route to the planning layer (separate plan).
        "daruma_ai_analyze_complexity" => {
            let plan_id = required_string(&args, "plan_id")?;
            let mut body = json!({});
            if let Some(flag) = args.get("use_research_provider").and_then(|v| v.as_bool()) {
                body["use_research_provider"] = json!(flag);
            }
            client
                .post_json(&format!("/v1/ai/analyze-complexity/{plan_id}"), body)
                .await
        }
        "daruma_comment" => {
            let task_id = required_string(&args, "task_id")?;
            let body_text = required_string(&args, "body")?;
            // §3.8.8: optional semantic classification. We validate locally
            // against the canonical set so MCP callers get an immediate
            // error rather than a server-side 400. The authoritative parser
            // lives in `daruma_domain::CommentKind::FromStr`, mirrored
            // here so the mcp crate doesn't need a domain dependency.
            let mut body_json = json!({"body": body_text});
            if let Some(kind_raw) = args.get("kind").and_then(|v| v.as_str()) {
                let normalised = normalise_comment_kind(kind_raw)?;
                body_json["kind"] = json!(normalised);
            }
            client
                .post_json(&format!("/v1/tasks/{task_id}/comments"), body_json)
                .await
        }
        "daruma_reopen" => {
            let id = required_string(&args, "id")?;
            client
                .post_command(json!({"type":"set_status","id": id, "status": "todo"}))
                .await
        }
        "daruma_inbox_pull" => {
            let agent_id = required_string(&args, "agent_id")?;
            let long_poll = args
                .get("long_poll_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let max = args.get("max").and_then(|v| v.as_u64()).unwrap_or(100);
            client
                .get_json(&format!(
                    "/v1/agents/{agent_id}/inbox?long_poll={long_poll}&max={max}"
                ))
                .await
        }
        "daruma_subscribe_project" => {
            // One-shot polling form: deliver any events whose target_project
            // matches the requested project. The streaming form lives on
            // `/v1/ws` (subscribe with `projects: [...]`). For MVP, we just
            // return the snapshot of recent events.
            let since = args.get("since_seq").and_then(|v| v.as_u64()).unwrap_or(0);
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(100);
            client
                .get_json(&format!("/v1/events?since={since}&limit={limit}"))
                .await
        }
        "daruma_events_since" => {
            let since = args.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(100);
            client
                .get_json(&format!("/v1/events?since={since}&limit={limit}"))
                .await
        }
        "daruma_healthz" => client.get_json("/v1/healthz").await,

        // ── Plan tools (W3.2) ─────────────────────────────────────────────
        "daruma_plan_create" => {
            let title = required_string(&args, "title")?;
            let project_id = required_string(&args, "project_id")?;
            // Server expects {plan: NewPlan, external_ref?}; NewPlan requires
            // an `owner: Actor` (we default to {kind: "user"}, matching the
            // /v1/plans e2e tests). Other fields are optional.
            let mut plan = json!({
                "title": title,
                "project_id": project_id,
                "owner": {"kind": "user"},
            });
            if let Some(desc) = args.get("description").and_then(|v| v.as_str()) {
                plan["description"] = json!(desc);
            }
            if let Some(goal) = args.get("goal").and_then(|v| v.as_str()) {
                plan["goal"] = json!(goal);
            }
            if let Some(parent) = args.get("parent_plan_id").and_then(|v| v.as_str()) {
                plan["parent_plan_id"] = json!(parent);
            }
            if let Some(criteria) = args.get("success_criteria") {
                plan["success_criteria"] = criteria.clone();
            }
            client.post_json("/v1/plans", json!({ "plan": plan })).await
        }
        "daruma_plan_update" => {
            let id = required_string(&args, "id")?;
            let patch = args
                .get("patch")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("`patch` (object) is required"))?;
            // Server expects {patch: PlanPatch}; wrap the patch payload.
            client
                .patch_json(&format!("/v1/plans/{id}"), json!({ "patch": patch }))
                .await
        }
        "daruma_plan_get" => {
            let id = required_string(&args, "id")?;
            let view = view_arg(&args, "progress", &["progress", "detail"])?;
            let resp = client.get_json(&format!("/v1/plans/{id}")).await?;
            if view == "detail" {
                Ok(resp)
            } else {
                let graph = client.get_json(&format!("/v1/plans/{id}/graph")).await?;
                Ok(plan_progress_view(resp, graph))
            }
        }
        "daruma_plan_list" => {
            let status = required_string(&args, "status")?;
            let view = view_arg(&args, "summary", &["summary", "detail"])?;
            let cursor = args.get("cursor").and_then(|v| v.as_str());
            let limit = mcp_collection_limit(&args);
            let mut params: Vec<(&str, String)> = vec![("status", urlencode(status.trim()))];
            match resolve_project_filter(client, &args, true, false, true).await? {
                ProjectFilter::All | ProjectFilter::None => {}
                ProjectFilter::Project(pid) => {
                    params.push(("project_id", urlencode(&pid)));
                }
            }
            let qs = params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            let resp = mcp_page_by_id(
                client.get_json(&format!("/v1/plans?{qs}")).await?,
                cursor,
                limit,
            );
            Ok(if view == "detail" {
                resp
            } else {
                summarize_rows_protected(
                    resp,
                    &[
                        "id",
                        "title",
                        "status",
                        "project_id",
                        "parent_plan_id",
                        "updated_at",
                    ],
                )
            })
        }
        "daruma_plan_add_task" => {
            let plan_id = required_string(&args, "plan_id")?;
            let task_id = required_string(&args, "task_id")?;
            let mut body = json!({"task_id": task_id});
            if let Some(pos) = args.get("position").and_then(|v| v.as_u64()) {
                body["position"] = json!(pos);
            }
            if let Some(deps) = args.get("depends_on").cloned() {
                body["depends_on"] = deps;
            }
            client
                .post_json(&format!("/v1/plans/{plan_id}/tasks"), body)
                .await
        }
        "daruma_plan_remove_task" => {
            let plan_id = required_string(&args, "plan_id")?;
            let task_id = required_string(&args, "task_id")?;
            client
                .delete_json(&format!("/v1/plans/{plan_id}/tasks/{task_id}"))
                .await
        }
        "daruma_plan_reorder" => {
            let plan_id = required_string(&args, "plan_id")?;
            let order = args
                .get("order")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("`order` (array of task ids) is required"))?;
            client
                .post_json(
                    &format!("/v1/plans/{plan_id}/reorder"),
                    json!({"order": order}),
                )
                .await
        }
        "daruma_plan_archive" => {
            let id = required_string(&args, "id")?;
            client
                .post_json(&format!("/v1/plans/{id}/archive"), json!({}))
                .await
        }
        "daruma_plan_set_status" => {
            let id = required_string(&args, "plan_id")?;
            let status = required_string(&args, "status")?;
            client
                .post_json(
                    &format!("/v1/plans/{id}/status"),
                    json!({ "status": status }),
                )
                .await
        }
        "daruma_plan_next_task" => {
            let id = required_string(&args, "id")?;
            let run_id = required_string(&args, "run_id")?;
            let ttl = args
                .get("claim_ttl_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            client
                .get_json(&format!(
                    "/v1/plans/{id}/next-task?run_id={run_id}&claim_ttl_secs={ttl}"
                ))
                .await
        }
        "daruma_plan_progress" => {
            let plan_id = required_string(&args, "plan_id")?;
            client
                .get_json(&format!("/v1/plans/{plan_id}/progress"))
                .await
        }
        "daruma_plan_drain_next" => {
            let plan_id = required_string(&args, "plan_id")?;
            let mut body = json!({});
            if let Some(run_id) = args.get("run_id").and_then(|v| v.as_str()) {
                body["run_id"] = json!(run_id);
            }
            if let Some(ttl) = args.get("claim_ttl_secs").and_then(|v| v.as_u64()) {
                body["claim_ttl_secs"] = json!(ttl);
            }
            client
                .post_json(&format!("/v1/plans/{plan_id}/drain-next"), body)
                .await
        }
        "daruma_plan_graph" => {
            let plan_id = required_string(&args, "plan_id")?;
            client.get_json(&format!("/v1/plans/{plan_id}/graph")).await
        }
        "daruma_plan_fanout" => {
            let plan_id = required_string(&args, "plan_id")?;
            client
                .get_json(&format!("/v1/plans/{plan_id}/fanout"))
                .await
        }
        "daruma_can_start" => {
            let task_id = required_string(&args, "task_id")?;
            client
                .get_json(&format!("/v1/tasks/{task_id}/can_start"))
                .await
        }

        // ── WorkspaceGraph tools (P3) ─────────────────────────────────────
        "daruma_workspacegraph_status" => client.get_json("/v1/workspacegraph/status").await,
        "daruma_workspacegraph_context" => {
            let node_id = required_string(&args, "node_id")?;
            let mut params: Vec<(&str, String)> = vec![("node_id", urlencode(&node_id))];
            if let Some(limit) = args.get("limit").and_then(|v| v.as_u64()) {
                params.push(("limit", limit.to_string()));
            }
            let qs = params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            client
                .get_json(&format!("/v1/workspacegraph/context?{qs}"))
                .await
        }
        "daruma_workspacegraph_related" => {
            let node_id = required_string(&args, "node_id")?;
            let mut params: Vec<(&str, String)> = vec![("node_id", urlencode(&node_id))];
            if let Some(depth) = args.get("depth").and_then(|v| v.as_u64()) {
                params.push(("depth", depth.to_string()));
            }
            if let Some(limit) = args.get("limit").and_then(|v| v.as_u64()) {
                params.push(("limit", limit.to_string()));
            }
            let qs = params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            client
                .get_json(&format!("/v1/workspacegraph/related?{qs}"))
                .await
        }
        "daruma_workspacegraph_search" => {
            let query = required_string(&args, "query")?;
            let limit = args.get("limit").and_then(|v| v.as_u64());
            let mut params: Vec<(&str, String)> = vec![("query", urlencode(&query))];
            match resolve_project_filter(client, &args, true, false, true).await? {
                ProjectFilter::All => params.push(("project_id", "all".to_string())),
                ProjectFilter::Project(pid) => params.push(("project_id", urlencode(&pid))),
                ProjectFilter::None => {}
            }
            if let Some(limit) = limit {
                params.push(("limit", limit.to_string()));
            }
            let qs = params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            client
                .get_json(&format!("/v1/workspacegraph/search?{qs}"))
                .await
        }
        "daruma_workspacegraph_impact" => {
            let node_id = required_string(&args, "node_id")?;
            let mut params: Vec<(&str, String)> = vec![("node_id", urlencode(&node_id))];
            if let Some(limit) = args.get("limit").and_then(|v| v.as_u64()) {
                params.push(("limit", limit.to_string()));
            }
            let qs = params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            client
                .get_json(&format!("/v1/workspacegraph/impact?{qs}"))
                .await
        }

        // ── Run tools (W3.2) ──────────────────────────────────────────────
        "daruma_run_start" => {
            let plan_id = required_string(&args, "plan_id")?;
            let agent_id = required_string(&args, "agent_id")?;
            let mut body = json!({"plan_id": plan_id, "agent_id": agent_id});
            if let Some(parent) = args.get("parent_run_id").and_then(|v| v.as_str()) {
                body["parent_run_id"] = json!(parent);
            }
            client.post_json("/v1/runs", body).await
        }
        "daruma_run_start_step" => {
            let run_id = required_string(&args, "run_id")?;
            let task_id = required_string(&args, "task_id")?;
            client
                .post_json(
                    &format!("/v1/runs/{run_id}/step/start"),
                    json!({"task_id": task_id}),
                )
                .await
        }
        "daruma_run_finish_step" => {
            let run_id = required_string(&args, "run_id")?;
            let task_id = required_string(&args, "task_id")?;
            let outcome = args.get("outcome").cloned().ok_or_else(|| {
                anyhow::anyhow!("`outcome` (object with `kind` field) is required")
            })?;
            client
                .post_json(
                    &format!("/v1/runs/{run_id}/step/finish"),
                    json!({"task_id": task_id, "outcome": outcome}),
                )
                .await
        }
        "daruma_run_complete" => {
            let run_id = required_string(&args, "run_id")?;
            client
                .post_json(&format!("/v1/runs/{run_id}/complete"), json!({}))
                .await
        }
        "daruma_run_abort" => {
            let run_id = required_string(&args, "run_id")?;
            let reason = args
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("user_requested")
                .to_string();
            client
                .post_json(
                    &format!("/v1/runs/{run_id}/abort"),
                    json!({"reason": reason}),
                )
                .await
        }

        // ── Run note tools (§3.8.2) ───────────────────────────────────────
        "daruma_run_note_append" => {
            let run_id = required_string(&args, "run_id")?;
            let body = required_string(&args, "body")?;
            client
                .post_json(&format!("/v1/runs/{run_id}/notes"), json!({"body": body}))
                .await
        }
        "daruma_run_log" => {
            let run_id = required_string(&args, "run_id")?;
            let level = args
                .get("level")
                .and_then(|v| v.as_str())
                .unwrap_or("info")
                .trim()
                .to_ascii_lowercase();
            let level = match level.as_str() {
                "debug" | "info" | "warn" | "error" => level,
                _ => "info".to_string(),
            };
            let body = required_string(&args, "body")?;
            client
                .post_json(
                    &format!("/v1/runs/{run_id}/notes"),
                    json!({"body": format!("[{level}] {body}")}),
                )
                .await
        }
        "daruma_run_notes_list" => {
            let run_id = required_string(&args, "run_id")?;
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50);
            let mut path = format!("/v1/runs/{run_id}/notes?limit={limit}");
            if let Some(after) = args.get("after").and_then(|v| v.as_str()) {
                path.push_str("&after=");
                path.push_str(after);
            }
            client.get_json(&path).await
        }

        // ── Claim tools (W3.2) ────────────────────────────────────────────
        "daruma_claim" => {
            let agent_id = required_string(&args, "agent_id")?;
            let task_id = required_string(&args, "task_id")?;
            let ttl_secs = args.get("ttl_secs").and_then(|v| v.as_u64()).unwrap_or(300);
            client
                .post_json(
                    "/v1/claims",
                    json!({"agent_id": agent_id, "task_id": task_id, "ttl_secs": ttl_secs}),
                )
                .await
        }
        "daruma_release" => {
            let agent_id = required_string(&args, "agent_id")?;
            let task_id = required_string(&args, "task_id")?;
            client
                .delete_json(&format!("/v1/claims/{agent_id}/{task_id}"))
                .await
        }

        // ── Work-lease tools (parallel-agent file coordination) ──────────
        "daruma_work_unit_create" => {
            let task_id = required_string(&args, "task_id")?;
            let title = required_string(&args, "title")?;
            let mut wu = json!({ "task_id": task_id, "title": title });
            for key in ["description", "stage_plan_id", "priority"] {
                if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
                    wu[key] = json!(v);
                }
            }
            for key in ["capability_tags", "artifact_refs", "acceptance"] {
                if let Some(v) = args.get(key).and_then(|v| v.as_array()) {
                    wu[key] = json!(v);
                }
            }
            client
                .post_json("/v1/work-units", json!({ "work_unit": wu }))
                .await
        }
        "daruma_work_unit_list" => {
            let task_id = required_string(&args, "task_id")?;
            client
                .get_json(&format!("/v1/tasks/{task_id}/work-units"))
                .await
        }
        "daruma_work_unit_drain_next" => {
            let task_id = required_string(&args, "task_id")?;
            let mut body = json!({ "task_id": task_id });
            if let Some(ttl) = args.get("ttl_secs").and_then(|v| v.as_u64()) {
                body["ttl_secs"] = json!(ttl);
            }
            client.post_json("/v1/work-units/drain-next", body).await
        }
        "daruma_work_unit_complete" => {
            let id = required_string(&args, "id")?;
            let mut body = json!({});
            if let Some(o) = args.get("outcome").and_then(|v| v.as_str()) {
                body["outcome"] = json!(o);
            }
            if let Some(a) = args.get("produced_artifacts").and_then(|v| v.as_array()) {
                body["produced_artifacts"] = json!(a);
            }
            if let Some(n) = args.get("next_suggested_units").and_then(|v| v.as_array()) {
                body["next_suggested_units"] = json!(n);
            }
            client
                .post_json(&format!("/v1/work-units/{id}/complete"), body)
                .await
        }
        "daruma_handoff_request" => {
            let mut handoff = json!({
                "from_work_unit_id": required_string(&args, "from_work_unit_id")?,
                "to_work_unit_id": required_string(&args, "to_work_unit_id")?,
            });
            for key in ["required_artifact_ids", "checklist"] {
                if let Some(v) = args.get(key).and_then(|v| v.as_array()) {
                    handoff[key] = json!(v);
                }
            }
            if let Some(v) = args.get("required_state").and_then(|v| v.as_str()) {
                handoff["required_state"] = json!(v);
            }
            client
                .post_json("/v1/handoffs", json!({ "handoff": handoff }))
                .await
        }
        "daruma_handoff_respond" => {
            let id = required_string(&args, "handoff_id")?;
            let decision = required_string(&args, "decision")?;
            match decision.as_str() {
                "accept" => {
                    let mut body = json!({});
                    if let Some(n) = args.get("notes").and_then(|v| v.as_str()) {
                        body["notes"] = json!(n);
                    }
                    client
                        .post_json(&format!("/v1/handoffs/{id}/accept"), body)
                        .await
                }
                "reject" => {
                    let reason = required_string(&args, "reason")?;
                    let mut body = json!({ "reason": reason });
                    if let Some(c) = args.get("required_changes").and_then(|v| v.as_array()) {
                        body["required_changes"] = json!(c);
                    }
                    client
                        .post_json(&format!("/v1/handoffs/{id}/reject"), body)
                        .await
                }
                other => anyhow::bail!("decision must be `accept` or `reject`, got {other:?}"),
            }
        }
        "daruma_handoff_list" => {
            let id = required_string(&args, "work_unit_id")?;
            client
                .get_json(&format!("/v1/work-units/{id}/handoffs"))
                .await
        }
        "daruma_work_unit_release" => {
            let id = required_string(&args, "id")?;
            client
                .post_json(&format!("/v1/work-units/{id}/release"), json!({}))
                .await
        }
        "daruma_project_settings_get" => {
            let project_id = required_string(&args, "project_id")?;
            client
                .get_json(&format!("/v1/projects/{project_id}/settings"))
                .await
        }
        "daruma_project_settings_update" => {
            let project_id = required_string(&args, "project_id")?;
            let mut auto_append = json!({});
            if let Some(v) = args.get("interview").and_then(|v| v.as_bool()) {
                auto_append["interview"] = json!(v);
            }
            if let Some(v) = args.get("human_log").and_then(|v| v.as_bool()) {
                auto_append["human_log"] = json!(v);
            }
            client
                .patch_json(
                    &format!("/v1/projects/{project_id}/settings"),
                    json!({ "auto_append": auto_append }),
                )
                .await
        }
        "daruma_rule_list" => {
            let mut qs = Vec::new();
            for key in ["project_id", "plan_id", "task_id"] {
                if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
                    qs.push(format!("{key}={v}"));
                }
            }
            let path = if qs.is_empty() {
                "/v1/rules".to_string()
            } else {
                format!("/v1/rules?{}", qs.join("&"))
            };
            client.get_json(&path).await
        }
        "daruma_rule_get" => {
            let id = required_string(&args, "id")?;
            client.get_json(&format!("/v1/rules/{id}")).await
        }
        "daruma_rule_create" => {
            let rule = args
                .get("rule")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("`rule` is required"))?;
            client.post_json("/v1/rules", json!({ "rule": rule })).await
        }
        "daruma_rule_update" => {
            let id = required_string(&args, "id")?;
            let mut patch = args.clone();
            patch.remove("id");
            client
                .patch_json(&format!("/v1/rules/{id}"), Value::Object(patch))
                .await
        }
        "daruma_rule_disable" => {
            let id = required_string(&args, "id")?;
            client.delete_json(&format!("/v1/rules/{id}")).await
        }
        "daruma_evidence_submit" => {
            let evidence = args
                .get("evidence")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("`evidence` is required"))?;
            client
                .post_json("/v1/evidence", json!({ "evidence": evidence }))
                .await
        }
        "daruma_evidence_list" => {
            let mut qs = Vec::new();
            for key in ["project_id", "plan_id", "task_id"] {
                if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
                    qs.push(format!("{key}={v}"));
                }
            }
            if let Some(true) = args.get("include_superseded").and_then(|v| v.as_bool()) {
                qs.push("include_superseded=true".to_string());
            }
            let path = if qs.is_empty() {
                "/v1/evidence".to_string()
            } else {
                format!("/v1/evidence?{}", qs.join("&"))
            };
            client.get_json(&path).await
        }
        // ── Audit primitives ─────────────────────────────────────────────
        "daruma_audit_findings" => {
            let project_id = required_string(&args, "project_id")?;
            let mut qs = vec![format!("project_id={project_id}")];
            for key in ["severity", "category", "status"] {
                if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
                    qs.push(format!("{key}={v}"));
                }
            }
            client
                .get_json(&format!("/v1/audit/findings?{}", qs.join("&")))
                .await
        }
        "daruma_audit_finding_ack" => {
            let id = required_string(&args, "id")?;
            let status = required_string(&args, "status")?;
            client
                .post_json(
                    &format!("/v1/audit/findings/{id}/status"),
                    json!({ "status": status }),
                )
                .await
        }
        "daruma_audit_stuck_tasks" => {
            let project_id = required_string(&args, "project_id")?;
            let mut qs = vec![format!("project_id={project_id}")];
            if let Some(s) = args.get("status").and_then(|v| v.as_str()) {
                qs.push(format!("status={s}"));
            }
            if let Some(h) = args.get("threshold_hours").and_then(|v| v.as_i64()) {
                qs.push(format!("threshold_hours={h}"));
            }
            client
                .get_json(&format!(
                    "/v1/audit/heuristics/stuck-tasks?{}",
                    qs.join("&")
                ))
                .await
        }
        "daruma_audit_duplicate_tasks" => {
            let project_id = required_string(&args, "project_id")?;
            let mut qs = vec![format!("project_id={project_id}")];
            if let Some(t) = args.get("threshold").and_then(|v| v.as_f64()) {
                qs.push(format!("threshold={t}"));
            }
            if let Some(l) = args.get("limit").and_then(|v| v.as_u64()) {
                qs.push(format!("limit={l}"));
            }
            client
                .get_json(&format!(
                    "/v1/audit/heuristics/duplicate-tasks?{}",
                    qs.join("&")
                ))
                .await
        }
        "daruma_audit_unread_documents" => {
            let project_id = required_string(&args, "project_id")?;
            let mut qs = vec![format!("project_id={project_id}")];
            if let Some(d) = args.get("days").and_then(|v| v.as_i64()) {
                qs.push(format!("days={d}"));
            }
            client
                .get_json(&format!(
                    "/v1/audit/heuristics/unread-documents?{}",
                    qs.join("&")
                ))
                .await
        }
        "daruma_workspace_resolve" => {
            let ws = workspace::global();
            let raw_path = args.get("scope_path").and_then(|v| v.as_str());
            let root_path = match (raw_path, ws) {
                (Some(p), Some(ws)) if !std::path::Path::new(p).is_absolute() => {
                    std::path::Path::new(ws.key())
                        .join(p)
                        .to_string_lossy()
                        .into_owned()
                }
                (Some(p), _) => p.to_string(),
                (None, Some(ws)) => ws.key().to_string(),
                (None, None) => anyhow::bail!("`scope_path` is required (no workspace state)"),
            };
            let mut body = json!({ "root_path": root_path });
            if let Some(c) = args.get("create").and_then(|v| v.as_bool()) {
                body["create"] = json!(c);
            }
            if let Some(w) = args.get("workspace_id").and_then(|v| v.as_str()) {
                body["workspace_id"] = json!(w);
            }
            let resp = client
                .post_json("/v1/workspace-registry/resolve", body)
                .await?;
            // Persist the resolved project as this scope's default so later
            // unscoped calls hit it without re-resolving.
            if let Some(project_id) = resp.get("project_id").and_then(|v| v.as_str()) {
                let _ = workspace::bind(client, &root_path, Some(project_id)).await;
            }
            Ok(resp)
        }
        "daruma_workspace_list" => client.get_json("/v1/workspace-registry").await,
        "daruma_project_move_workspace" => {
            let project_id = required_string(&args, "project_id")?;
            let workspace_id = required_string(&args, "workspace_id")?;
            let mut body = json!({ "workspace_id": workspace_id });
            if let Some(r) = args.get("root_path").and_then(|v| v.as_str()) {
                body["root_path"] = json!(r);
            }
            client
                .patch_json(&format!("/v1/projects/{project_id}/workspace"), body)
                .await
        }
        "daruma_reserve_files" => {
            let agent_id = required_string(&args, "agent_id")?;
            let task_id = required_string(&args, "task_id")?;
            let paths = args.get("paths").and_then(|v| v.as_array()).cloned();
            let targets = args.get("targets").and_then(|v| v.as_array()).cloned();
            if paths.as_ref().is_none_or(|p| p.is_empty())
                && targets.as_ref().is_none_or(|t| t.is_empty())
            {
                anyhow::bail!("`paths` and/or `targets` (array of strings) is required");
            }
            let mut body = json!({
                "agent_id": agent_id,
                "task_id": task_id,
            });
            if let Some(p) = paths {
                body["paths"] = json!(p);
            }
            if let Some(t) = targets {
                body["targets"] = json!(t);
            }
            if let Some(m) = args.get("mode").and_then(|v| v.as_str()) {
                body["mode"] = json!(m);
            }
            if let Some(p) = args.get("project_id").and_then(|v| v.as_str()) {
                body["project_id"] = json!(p);
            }
            if let Some(ttl) = args.get("ttl_secs").and_then(|v| v.as_u64()) {
                body["ttl_secs"] = json!(ttl);
            }
            client.post_json("/v1/leases", body).await
        }
        "daruma_release_files" => {
            let agent_id = required_string(&args, "agent_id")?;
            let task_id = required_string(&args, "task_id")?;
            client
                .delete_json(&format!("/v1/leases/{agent_id}/{task_id}"))
                .await
        }
        "daruma_active_work" => {
            let path = match args.get("project_id").and_then(|v| v.as_str()) {
                Some(p) if !p.is_empty() => format!("/v1/leases?project_id={p}"),
                _ => "/v1/leases".to_string(),
            };
            client.get_json(&path).await
        }

        // ── Project-wide ready pool ──────────────────────────────────────
        "daruma_ready" => {
            let project_id = required_string(&args, "project_id")?;
            client
                .get_json(&format!("/v1/ready?project_id={project_id}"))
                .await
        }
        "daruma_ready_drain" => {
            let project_id = required_string(&args, "project_id")?;
            let mut body = json!({});
            if let Some(ttl) = args.get("claim_ttl_secs").and_then(|v| v.as_u64()) {
                body["claim_ttl_secs"] = json!(ttl);
            }
            client
                .post_json(&format!("/v1/ready/drain?project_id={project_id}"), body)
                .await
        }
        "daruma_doctor" => {
            let project_id = required_string(&args, "project_id")?;
            client
                .get_json(&format!("/v1/doctor?project_id={project_id}"))
                .await
        }
        "daruma_suggest_files" => {
            let task_id = required_string(&args, "task_id")?;
            client
                .get_json(&format!("/v1/leases/suggest?task_id={task_id}"))
                .await
        }

        // ── Session tools (W3.2 / Linear B.1) ────────────────────────────
        "daruma_session_start" => {
            let agent_id = args
                .get("agent_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| client.agent_id());
            let mut body = json!({"agent_id": agent_id});
            if let Some(parent) = args.get("parent_agent_id").and_then(|v| v.as_str()) {
                body["parent_agent_id"] = json!(parent);
            }
            let metadata = args.get("metadata").cloned().unwrap_or_else(|| json!({}));
            body["metadata"] = session_metadata::merge_defaults(metadata);
            client.post_json("/v1/sessions", body).await
        }
        "daruma_session_get" => {
            let id = required_string(&args, "id")?;
            client.get_json(&format!("/v1/sessions/{id}")).await
        }
        "daruma_session_list" => {
            let agent_id = args
                .get("agent_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| client.agent_id());
            client
                .get_json(&format!("/v1/sessions?agent_id={agent_id}"))
                .await
        }
        "daruma_session_end" => {
            let id = required_string(&args, "id")?;
            client
                .post_json(&format!("/v1/sessions/{id}/end"), json!({}))
                .await
        }
        "daruma_session_set_plan" => {
            let id = required_string(&args, "id")?;
            let steps = args
                .get("steps")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("`steps` (array, max 100) is required"))?;
            client
                .post_json(&format!("/v1/sessions/{id}/plan"), json!({"steps": steps}))
                .await
        }
        "daruma_session_artifact" => {
            let session_id = required_string(&args, "session_id")?;
            let kind = required_string(&args, "kind")?;
            let reference = required_string(&args, "ref")?;
            let mut body = json!({"kind": kind, "ref": reference});
            if let Some(metadata) = args.get("metadata").cloned() {
                body["metadata"] = metadata;
            }
            client
                .post_json(&format!("/v1/sessions/{session_id}/artifacts"), body)
                .await
        }
        "daruma_session_artifacts_list" => {
            let id = required_string(&args, "id")?;
            client
                .get_json(&format!("/v1/sessions/{id}/artifacts"))
                .await
        }

        // ── Signal tools (W3.2 / Linear B.5) ─────────────────────────────
        "daruma_signal_send" => {
            let run_id = required_string(&args, "run_id")?;
            let kind = args
                .get("kind")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("`kind` (signal object) is required"))?;
            client
                .post_json(&format!("/v1/runs/{run_id}/signals"), json!({"kind": kind}))
                .await
        }
        "daruma_signal_respond" => {
            let run_id = required_string(&args, "run_id")?;
            let choice = required_string(&args, "choice")?;
            client
                .post_json(
                    &format!("/v1/runs/{run_id}/signals/respond"),
                    json!({"choice": choice}),
                )
                .await
        }

        // ── Relation tools (§3.2 W3.2) ───────────────────────────────────
        "daruma_link" => {
            let from = required_string(&args, "from")?;
            let to = required_string(&args, "to")?;
            let kind = required_string(&args, "kind")?;
            let mut body = serde_json::json!({"from": from, "to": to, "kind": kind});
            if let Some(ccid) = args.get("client_command_id").and_then(|v| v.as_str()) {
                body["client_command_id"] = serde_json::json!(ccid);
            }
            client.post_json("/v1/relations", body).await
        }
        "daruma_unlink" => {
            let relation_id = required_string(&args, "relation_id")?;
            client
                .delete_json(&format!("/v1/relations/{relation_id}"))
                .await
        }
        "daruma_relations" => {
            let task_id = required_string(&args, "task_id")?;
            client
                .get_json(&format!("/v1/tasks/{task_id}/relations"))
                .await
        }

        // ── Document tools (PR1 §7) ───────────────────────────────────────
        "daruma_doc_create" => {
            let project_id = required_string(&args, "project_id")?;
            let kind = required_string(&args, "kind")?;
            let title = required_string(&args, "title")?;
            let mut new_doc = json!({
                "project_id": project_id,
                "kind": kind,
                "title": title,
            });
            if let Some(content) = args.get("content").and_then(|v| v.as_str()) {
                new_doc["content"] = json!(content);
            }
            for key in ["status", "task_id", "trigger_kind", "consumer"] {
                if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
                    new_doc[key] = json!(v);
                }
            }
            client
                .post_json("/v1/documents", json!({ "new_doc": new_doc }))
                .await
        }
        "daruma_doc_get" => {
            let id = required_string(&args, "document_id")?;
            client.get_json(&format!("/v1/documents/{id}")).await
        }
        "daruma_doc_append" => {
            let id = required_string(&args, "document_id")?;
            let content = required_string(&args, "content")?;
            client
                .post_json(
                    &format!("/v1/documents/{id}/append"),
                    json!({ "content": content }),
                )
                .await
        }
        "daruma_doc_replace" => {
            let id = required_string(&args, "document_id")?;
            let content = required_string(&args, "content")?;
            client
                .patch_json(
                    &format!("/v1/documents/{id}"),
                    json!({ "content": content }),
                )
                .await
        }
        "daruma_doc_rename" => {
            let id = required_string(&args, "document_id")?;
            let title = required_string(&args, "title")?;
            client
                .patch_json(&format!("/v1/documents/{id}"), json!({ "title": title }))
                .await
        }
        "daruma_doc_archive" => {
            let id = required_string(&args, "document_id")?;
            client
                .post_json(&format!("/v1/documents/{id}/archive"), json!({}))
                .await
        }
        "daruma_doc_set_status" => {
            let id = required_string(&args, "document_id")?;
            let status = required_string(&args, "status")?;
            client
                .patch_json(&format!("/v1/documents/{id}"), json!({ "status": status }))
                .await
        }
        "daruma_doc_link_task" => {
            let id = required_string(&args, "document_id")?;
            // Explicit `task_id: null` (or absent) unlinks — the PATCH body
            // distinguishes present-null from absent, so always send the key.
            let task_id = args.get("task_id").cloned().unwrap_or(Value::Null);
            client
                .patch_json(
                    &format!("/v1/documents/{id}"),
                    json!({ "task_id": task_id }),
                )
                .await
        }
        "daruma_doc_list" => {
            // `project_id` falls back to the workspace default. The URL
            // path requires a project id, so we bail with a friendly error
            // if neither is set instead of producing a malformed URL.
            let project_id = match resolve_project_filter(client, &args, false, false, true).await? {
                ProjectFilter::Project(pid) => pid,
                ProjectFilter::None => {
                    anyhow::bail!(
                        "`project_id`, `project_scope`, or `scope_path` is required and no daruma scope is resolved"
                    )
                }
                ProjectFilter::All => unreachable!("allow_all=false"),
            };
            let mut qs = String::new();
            if let Some(kind) = args.get("kind").and_then(|v| v.as_str()) {
                qs.push_str(&format!("kind={}&", urlencode(kind)));
            }
            let include_archived = args
                .get("include_archived")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            qs.push_str(&format!("include_archived={include_archived}"));
            client
                .get_json(&format!("/v1/projects/{project_id}/documents?{qs}"))
                .await
        }

        // ── Version-history tools ─────────────────────────────────────────
        "daruma_history_list" => {
            let entity_type = required_string(&args, "entity_type")?;
            let entity_id = required_string(&args, "entity_id")?;
            let limit = optional_u32(&args, "limit").unwrap_or(50);
            client
                .get_json(&format!(
                    "/v1/history?entity_type={}&entity_id={}&limit={limit}",
                    urlencode(&entity_type),
                    urlencode(&entity_id)
                ))
                .await
        }
        "daruma_history_get" => {
            let id = required_string(&args, "version_id")?;
            client.get_json(&format!("/v1/history/{id}")).await
        }
        "daruma_history_compare" => {
            let entity_type = required_string(&args, "entity_type")?;
            let entity_id = required_string(&args, "entity_id")?;
            let from = required_i64(&args, "from")?;
            let to = required_i64(&args, "to")?;
            client
                .get_json(&format!(
                    "/v1/history/compare?entity_type={}&entity_id={}&from={from}&to={to}",
                    urlencode(&entity_type),
                    urlencode(&entity_id)
                ))
                .await
        }
        "daruma_history_latest" => {
            let limit = optional_u32(&args, "limit").unwrap_or(50);
            client
                .get_json(&format!("/v1/history/latest?limit={limit}"))
                .await
        }
        "daruma_history_summary" => {
            let entity_type = required_string(&args, "entity_type")?;
            let entity_id = required_string(&args, "entity_id")?;
            let limit = optional_u32(&args, "limit").unwrap_or(50);
            client
                .get_json(&format!(
                    "/v1/history/summary?entity_type={}&entity_id={}&limit={limit}",
                    urlencode(&entity_type),
                    urlencode(&entity_id)
                ))
                .await
        }
        "daruma_history_rollback" => {
            let id = required_string(&args, "version_id")?;
            client
                .post_json(&format!("/v1/history/{id}/rollback"), json!({}))
                .await
        }

        // ── Artifact Registry (P4) ───────────────────────────────────────
        "daruma_artifact_register" => {
            let uri = required_string(&args, "uri")?;
            let title = required_string(&args, "title")?;
            let mut body = json!({"uri": uri, "title": title});
            for key in ["description", "task_id", "project_id"] {
                if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
                    body[key] = json!(v);
                }
            }
            client.post_json("/v1/artifacts", body).await
        }
        "daruma_artifact_list" => {
            let mut params: Vec<(&str, String)> = vec![];
            if let Some(p) = args.get("project_id").and_then(|v| v.as_str()) {
                params.push(("project_id", urlencode(p)));
            }
            if let Some(t) = args.get("task_id").and_then(|v| v.as_str()) {
                params.push(("task_id", urlencode(t)));
            }
            if let Some(s) = args.get("status").and_then(|v| v.as_str()) {
                params.push(("status", urlencode(s)));
            }
            let qs = params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            let path = if qs.is_empty() {
                "/v1/artifacts".to_string()
            } else {
                format!("/v1/artifacts?{qs}")
            };
            client.get_json(&path).await
        }
        "daruma_artifact_impact" => {
            let artifact_id = required_string(&args, "artifact_id")?;
            let mut params: Vec<(&str, String)> =
                vec![("node_id", urlencode(&format!("artifact:{artifact_id}")))];
            if let Some(limit) = args.get("limit").and_then(|v| v.as_u64()) {
                params.push(("limit", limit.to_string()));
            }
            let qs = params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            client
                .get_json(&format!("/v1/workspacegraph/impact?{qs}"))
                .await
        }

        other => anyhow::bail!("unknown tool: {other}"),
    }
}

// ── schema builders ──────────────────────────────────────────────────────────

fn empty_schema() -> Value {
    json!({"type":"object","properties":{}})
}

fn schema_with_id(field: &str) -> Value {
    json!({
        "type":"object",
        "properties": {field: {"type":"string","description":"Task identifier"}},
        "required": [field]
    })
}

/// Complete a task with an optional completion note. The note fields are all
/// optional and omittable — calling with just `id` is the legacy behaviour.
fn schema_complete() -> Value {
    json!({
        "type":"object",
        "properties": {
            "id": {"type":"string","description":"Task identifier"},
            "reason": {"type":"string","description":"Optional: why the task is done."},
            "result_summary": {"type":"string","description":"Optional: what was produced / the outcome."},
            "acceptance_criteria_status": {"type":"string","description":"Optional: e.g. \"3/3 met\", \"AC2 waived\"."},
            "related_artifacts": {"type":"array","items":{"type":"string"},"description":"Optional: paths/URLs/doc refs/PR links produced."}
        },
        "required": ["id"]
    })
}

fn schema_rule_list() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {"type":"string","description":"List rules at this project scope."},
            "plan_id": {"type":"string","description":"List rules at this plan scope."},
            "task_id": {"type":"string","description":"List rules at this task scope."}
        }
    })
}

fn schema_rule_create() -> Value {
    json!({
        "type":"object",
        "properties": {
            "rule": {
                "type":"object",
                "description":"Lifecycle rule (see docs/LIFECYCLE_RULES_SPEC.md).",
                "properties": {
                    "rule_key": {"type":"string","description":"Stable key for inheritance/override, e.g. completion-note."},
                    "title": {"type":"string"},
                    "scope": {"type":"object","description":"{\"kind\":\"tenant\"} | {\"kind\":\"project\",\"id\":...} | plan | task."},
                    "trigger": {"type":"string","enum":["project.created","plan.created","plan.before_approve","task.created","task.before_start","task.before_complete","run.before_execute","run.before_complete"]},
                    "condition": {"type":["object","null"],"description":"Optional targeting: status_from/status_to."},
                    "requirement": {"type":"object","description":"Tagged by `type` (read_artifact, impact_check, completion_note, …)."},
                    "mode": {"type":"string","enum":["off","recommendation","required"]},
                    "message": {"type":"string"},
                    "override_allowed": {"type":"boolean"},
                    "enabled": {"type":"boolean"}
                },
                "required": ["rule_key","title","scope","trigger","requirement"]
            }
        },
        "required": ["rule"]
    })
}

fn schema_rule_update() -> Value {
    json!({
        "type":"object",
        "properties": {
            "id": {"type":"string","description":"Rule identifier."},
            "title": {"type":"string"},
            "condition": {"type":["object","null"]},
            "requirement": {"type":"object"},
            "mode": {"type":"string","enum":["off","recommendation","required"]},
            "message": {"type":"string"},
            "override_allowed": {"type":"boolean"},
            "enabled": {"type":"boolean"}
        },
        "required": ["id"]
    })
}

fn schema_evidence_submit() -> Value {
    json!({
        "type":"object",
        "properties": {
            "evidence": {
                "type":"object",
                "description":"Evidence record (immutable). See the evidence registry.",
                "properties": {
                    "kind": {"type":"string","enum":["document_read_ack","impact_assessment","decision_record","completion_note","artifact_created","owner_assigned","acceptance_criteria_defined","risk_check_completed"]},
                    "scope": {"type":"object","description":"{\"kind\":\"tenant\"} | {\"kind\":\"project\",\"id\":...} | plan | task."},
                    "target": {"type":"string","description":"Optional discriminator matching a requirement target / doc_ref; omit to satisfy any target."},
                    "doc_version": {"type":"string","description":"For document_read_ack: the document version that was read."},
                    "reason": {"type":"string"},
                    "payload": {"description":"Optional structured payload (any JSON)."},
                    "project_id": {"type":"string"},
                    "plan_id": {"type":"string"},
                    "task_id": {"type":"string"},
                    "run_id": {"type":"string"},
                    "artifact_id": {"type":"string"},
                    "rule_id": {"type":"string"},
                    "supersedes": {"type":"string","description":"Id of an earlier record this one supersedes (immutability: the old row is marked, not edited)."}
                },
                "required": ["kind","scope"]
            }
        },
        "required": ["evidence"]
    })
}

fn schema_evidence_list() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {"type":"string","description":"List evidence at this project scope."},
            "plan_id": {"type":"string","description":"List evidence at this plan scope."},
            "task_id": {"type":"string","description":"List evidence at this task scope."},
            "include_superseded": {"type":"boolean","description":"Include superseded records (default false)."}
        }
    })
}

fn schema_audit_findings() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {"type":"string","description":"Project to list findings for."},
            "severity": {"type":"string","enum":["error","warn","info"],"description":"Filter by severity."},
            "category": {"type":"string","description":"Filter by category bucket."},
            "status": {"type":"string","enum":["open","acknowledged","muted","resolved"],"description":"Filter by status."}
        },
        "required": ["project_id"]
    })
}

fn schema_audit_finding_ack() -> Value {
    json!({
        "type":"object",
        "properties": {
            "id": {"type":"string","description":"Finding id."},
            "status": {"type":"string","enum":["open","acknowledged","muted","resolved"],"description":"New status."}
        },
        "required": ["id","status"]
    })
}

fn schema_audit_stuck_tasks() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {"type":"string","description":"Project to inspect."},
            "status": {"type":"string","enum":["inbox","todo","in_progress","in_review","done","cancelled"],"description":"Status to inspect (default in_progress)."},
            "threshold_hours": {"type":"integer","description":"Stuck threshold in hours (default 72)."}
        },
        "required": ["project_id"]
    })
}

fn schema_audit_duplicate_tasks() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {"type":"string","description":"Project to inspect."},
            "threshold": {"type":"number","description":"bm25 threshold; pairs with rank <= this are returned (lower = stronger; default -1.0)."},
            "limit": {"type":"integer","description":"Per-task candidate cap (default 20)."}
        },
        "required": ["project_id"]
    })
}

fn schema_audit_unread_documents() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {"type":"string","description":"Project to inspect."},
            "days": {"type":"integer","description":"Days since last read (default 30); never-read documents always qualify."}
        },
        "required": ["project_id"]
    })
}

fn schema_with_plan_id() -> Value {
    json!({
        "type":"object",
        "properties": {"plan_id": {"type":"string","description":"Plan identifier"}},
        "required": ["plan_id"]
    })
}

fn schema_create() -> Value {
    json!({
        "type":"object",
        "properties": {
            "task": {
                "type":"object",
                "properties": {
                    "title": {"type":"string"},
                    "description": {"type":"string"},
                    "status": {"type":"string","enum":["inbox","todo","in_progress","done"]},
                    "priority": {"type":"string","enum":["p0","p1","p2","p3"]},
                    "project_id": {"type":"string"},
                    "external_key": {"type":"string","description":"Optional idempotency key from an external source (webhook/importer). Unique within the workspace: re-creating with the same key does not duplicate the task — the incoming context is appended as a comment to the existing task instead."}
                },
                "required":["title"]
            },
            "scope": {
                "type":"string",
                "description":"Named daruma scope (usually repo folder name) used when task.project_id is omitted."
            },
            "project_scope": {
                "type":"string",
                "description":"Named daruma scope (alias-safe form; preferred when a tool already has a `scope` option)."
            },
            "scope_path": {
                "type":"string",
                "description":"Filesystem path used to resolve the nearest configured daruma scope."
            }
        },
        "required":["task"]
    })
}

fn schema_update() -> Value {
    json!({
        "type":"object",
        "properties": {
            "id": {"type":"string", "description":"Task identifier"},
            "title": {"type":"string"},
            "description": {"type":"string"},
            "due_at": {
                "description":"RFC3339 timestamp to set, or null to clear.",
                "anyOf": [{"type":"string"}, {"type":"null"}]
            }
        },
        "required":["id"]
    })
}

fn schema_capture() -> Value {
    json!({
        "type":"object",
        "properties": {
            "text": {"type":"string", "description":"Task title (the captured idea)."},
            "project_id": {
                "description":"Optional project scope. Omitted uses the resolved repo project when unambiguous; null means inbox-only.",
                "anyOf": [{"type":"string"}, {"type":"null"}]
            },
            "scope": {"type":"string", "description":"Named daruma scope (usually repo folder name)."},
            "project_scope": {"type":"string", "description":"Named daruma scope (alias-safe form)."},
            "scope_path": {"type":"string", "description":"Filesystem path used to resolve the nearest configured daruma scope."}
        },
        "required":["text"]
    })
}

fn schema_capture_batch() -> Value {
    json!({
        "type":"object",
        "properties": {
            "texts": {
                "type":"array",
                "items": {"type":"string"},
                "minItems": 1,
                "description":"Each string becomes a separate inbox task."
            },
            "project_id": {
                "description":"Optional project scope. Omitted uses the resolved repo project when unambiguous; null means inbox-only.",
                "anyOf": [{"type":"string"}, {"type":"null"}]
            },
            "scope": {"type":"string", "description":"Named daruma scope (usually repo folder name)."},
            "project_scope": {"type":"string", "description":"Named daruma scope (alias-safe form)."},
            "scope_path": {"type":"string", "description":"Filesystem path used to resolve the nearest configured daruma scope."}
        },
        "required":["texts"]
    })
}

fn schema_set_status() -> Value {
    json!({
        "type":"object",
        "properties": {
            "id": {"type":"string"},
            "status": {"type":"string","enum":["inbox","todo","in_progress","in_review","done","cancelled"]},
            "force": {
                "type":"boolean",
                "description":"When setting in_progress, suppress the soft can_start warning for actively blocked tasks."
            }
        },
        "required":["id","status"]
    })
}

fn schema_set_priority() -> Value {
    json!({
        "type":"object",
        "properties": {
            "id": {"type":"string"},
            "priority": {"type":"string","enum":["p0","p1","p2","p3"]}
        },
        "required":["id","priority"]
    })
}

fn schema_move_project() -> Value {
    json!({
        "type":"object",
        "properties": {
            "id": {"type":"string", "description":"Task id to move."},
            "project_id": {"type":"string", "description":"Destination project id."},
            "scope": {"type":"string", "description":"Destination daruma scope (usually repo folder name)."},
            "project_scope": {"type":"string", "description":"Destination daruma scope (alias-safe form)."},
            "scope_path": {"type":"string", "description":"Filesystem path used to resolve the destination daruma scope."}
        },
        "required":["id"]
    })
}

fn schema_split() -> Value {
    json!({
        "type":"object",
        "properties": {
            "parent": {"type":"string"},
            "subtasks": {"type":"array","items":{"type":"object"}}
        },
        "required":["parent","subtasks"]
    })
}

fn schema_bulk_set_status() -> Value {
    json!({
        "type":"object",
        "properties": {
            "ids": {
                "type":"array",
                "items":{"type":"string"},
                "minItems":1,
                "maxItems":50
            },
            "status": {
                "type":"string",
                "enum":["inbox","todo","in_progress","in_review","done","cancelled"]
            }
        },
        "required":["ids","status"]
    })
}

fn schema_bulk_attach_to_plan() -> Value {
    json!({
        "type":"object",
        "properties": {
            "plan_id": {"type":"string"},
            "ids": {
                "type":"array",
                "items":{"type":"string"},
                "minItems":1,
                "maxItems":50,
                "description":"Task ids to attach. Aliased as `task_ids` for backward symmetry."
            }
        },
        "required":["plan_id","ids"]
    })
}

/// §3.8.13: per-tool `use_research_provider` flag. Currently silently
/// ignored by the server (single provider); kept in the public schema
/// so callers can author against the final shape ahead of §3.8.9.
fn use_research_provider_property() -> Value {
    json!({
        "type": "boolean",
        "description": "Opt into a future research-capable provider for this call. Currently a no-op (single provider); accepted for forward-compat with §3.8.9."
    })
}

fn schema_ai_analyze_complexity() -> Value {
    json!({
        "type":"object",
        "properties": {
            "plan_id": {
                "type":"string",
                "description":"Plan whose tasks should be scored. Every task in the plan is included in one batch LLM call."
            },
            "use_research_provider": use_research_provider_property(),
        },
        "required":["plan_id"]
    })
}

fn schema_comment() -> Value {
    json!({
        "type":"object",
        "properties": {
            "task_id": {"type":"string"},
            "body": {"type":"string"},
            // §3.8.8: optional semantic classification. Canonical form is
            // snake_case; the server is lenient about case.
            "kind": {
                "type":"string",
                "enum":[
                    "intent","progress","outcome","blocker","research",
                    "Intent","Progress","Outcome","Blocker","Research"
                ],
                "description":"Optional semantic kind (Intent/Progress/Outcome/Blocker/Research)."
            }
        },
        "required":["task_id","body"]
    })
}

fn schema_inbox_pull() -> Value {
    json!({
        "type":"object",
        "properties": {
            "agent_id": {"type":"string"},
            "long_poll_secs": {"type":"integer","minimum":0,"maximum":60},
            "max": {"type":"integer","minimum":1,"maximum":1000}
        },
        "required":["agent_id"]
    })
}

fn schema_subscribe_project() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {"type":"string"},
            "since_seq": {"type":"integer","minimum":0},
            "limit": {"type":"integer","minimum":1,"maximum":1000}
        }
    })
}

fn schema_list() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {
                "type":"string",
                "description": "Either a project id, `inbox` for tasks with no project, or `all` to ignore repo inference. When omitted, the resolved repo project is used only if unambiguous."
            },
            "scope": {
                "type":"string",
                "description":"Named daruma scope (usually repo folder name)."
            },
            "project_scope": {
                "type":"string",
                "description":"Named daruma scope (alias-safe form)."
            },
            "scope_path": {
                "type":"string",
                "description":"Filesystem path used to resolve the nearest configured daruma scope."
            },
            "status": {
                "type":"string",
                "description": "Required. Single status (`inbox`/`todo`/`in_progress`/`in_review`/`done`/`cancelled`), comma-separated list (e.g. `todo,in_progress`), shortcut `active` (non-terminal), or `all`. **Ask the user before `all`** — full archive can be a very heavy response."
            },
            "limit": {
                "type":"integer",
                "minimum":1,
                "maximum":500,
                "default":10
            },
            "cursor": {
                "type":"string",
                "description":"Opaque next_cursor from the previous response. Never auto-fetch it without user intent."
            },
            "view": {
                "type":"string",
                "enum":["summary","detail"],
                "default":"summary",
                "description":"summary returns id/title/status/priority/project only; detail returns the legacy full task rows."
            }
        },
        "required": ["status"]
    })
}

fn schema_search() -> Value {
    json!({
        "type":"object",
        "properties": {
            "query": {"type":"string"},
            "scope": {
                "type":"string",
                "description":"Comma-separated subset of `tasks`, `comments`, `plans`. Omit for all."
            },
            "project_id": {
                "type":"string",
                "description":"Project id, or `all` for every project. Omitted uses the resolved repo project when unambiguous."
            },
            "project_scope": {
                "type":"string",
                "description":"Named daruma scope (use this instead of `scope`, which filters search domains)."
            },
            "scope_path": {
                "type":"string",
                "description":"Filesystem path used to resolve the nearest configured daruma scope."
            },
            "limit": {
                "type":"integer",
                "minimum":1,
                "maximum":500,
                "default":10
            },
            "cursor": {
                "type":"string",
                "description":"Opaque next_cursor from the previous response. Never auto-fetch it without user intent."
            },
            "view": {
                "type":"string",
                "enum":["summary","detail"],
                "default":"summary",
                "description":"summary returns compact hit rows; detail returns the legacy full search hits."
            }
        },
        "required":["query"]
    })
}

fn schema_lesson_recall() -> Value {
    json!({
        "type":"object",
        "properties": {
            "query": {
                "type":"string",
                "description":"Optional text immediately after the `lesson:` comment prefix."
            },
            "project_id": {
                "type":"string",
                "description":"Project id, or `all` for every project. Omitted uses the resolved repo project when unambiguous."
            },
            "project_scope": {
                "type":"string",
                "description":"Named daruma scope."
            },
            "scope_path": {
                "type":"string",
                "description":"Filesystem path used to resolve the nearest configured daruma scope."
            },
            "limit": {
                "type":"integer",
                "minimum":1,
                "maximum":100,
                "default":20
            }
        }
    })
}

fn schema_project_create() -> Value {
    json!({
        "type":"object",
        "properties": {
            "title": {"type":"string"},
            "description": {"type":"string"}
        },
        "required":["title"]
    })
}

fn schema_project_delete() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "description": "Project id to delete."
            },
            "confirm": {
                "type": "string",
                "description": "Must match the project's exact title (case-sensitive). Required on the second call."
            },
            "confirm_token": {
                "type": "string",
                "description": "One-time token issued by the first call. Required on the second call."
            }
        },
        "required": ["id"]
    })
}

fn schema_project_use() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {
                "type": ["string", "null"],
                "description": "Project id, or null to clear the selected workspace/repo scope."
            },
            "scope_path": {
                "type": "string",
                "description": "Workspace or repository path to bind. Relative paths are resolved from DARUMA_WORKSPACE / process CWD. Omit only when MCP is running inside the repository scope."
            }
        },
        "required":["project_id"]
    })
}

fn urlencode(raw: &str) -> String {
    // Tiny percent-encoder — adequate for our id alphabet (UUIDs, prefixed
    // hex, the literal `inbox`/`all`). Avoids pulling in a full url crate.
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn schema_events_since() -> Value {
    json!({
        "type":"object",
        "properties": {
            "seq":   {"type":"integer","minimum":0},
            "limit": {"type":"integer","minimum":1,"maximum":1000}
        }
    })
}

// ── Plan schemas (W3.2) ──────────────────────────────────────────────────────

fn schema_plan_create() -> Value {
    json!({
        "type":"object",
        "properties": {
            "title":            {"type":"string"},
            "project_id":       {"type":"string"},
            "description":      {"type":"string"},
            "goal":             {"type":"string"},
            "parent_plan_id":   {"type":"string"},
            "success_criteria": {"type":"array","items":{"type":"string"}}
        },
        "required":["title","project_id"]
    })
}

fn schema_plan_update() -> Value {
    json!({
        "type":"object",
        "properties": {
            "id": {"type":"string"},
            "patch": {
                "type":"object",
                "properties": {
                    "title":            {"type":"string"},
                    "description":      {"type":"string"},
                    "goal":             {"type":"string"},
                    "success_criteria": {"type":"array","items":{"type":"string"}},
                    "parent_plan_id": {
                        "type": ["string", "null"],
                        "description": "Set parent plan id; null to unparent (drop to root); omit to keep current."
                    }
                }
            }
        },
        "required":["id","patch"]
    })
}

fn schema_plan_set_status() -> Value {
    json!({
        "type":"object",
        "properties": {
            "plan_id": {"type":"string"},
            "status":  {"type":"string","enum":["draft","active","completed","abandoned"]}
        },
        "required":["plan_id","status"]
    })
}

fn schema_plan_get() -> Value {
    json!({
        "type":"object",
        "properties": {
            "id": {"type":"string"},
            "view": {
                "type":"string",
                "enum":["progress","detail"],
                "default":"progress",
                "description":"progress returns compact plan identity, counts, and active/blocked/next task titles; detail returns the legacy full {plan, progress} response."
            }
        },
        "required":["id"]
    })
}

fn schema_plan_list() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {
                "type":"string",
                "description":"Project id, or `all` to ignore repo inference. When omitted, the resolved repo project is used only if unambiguous."
            },
            "scope": {
                "type":"string",
                "description":"Named daruma scope (usually repo folder name)."
            },
            "project_scope": {
                "type":"string",
                "description":"Named daruma scope (alias-safe form)."
            },
            "scope_path": {
                "type":"string",
                "description":"Filesystem path used to resolve the nearest configured daruma scope."
            },
            "status": {
                "type":"string",
                "description": "Required. `draft`/`active`/`completed`/`abandoned`, comma-separated list, or `all`. **Ask the user before `all`** — full archive can be a very heavy response."
            },
            "limit": {
                "type":"integer",
                "minimum":1,
                "maximum":500,
                "default":10
            },
            "cursor": {
                "type":"string",
                "description":"Opaque next_cursor from the previous response. Never auto-fetch it without user intent."
            },
            "view": {
                "type":"string",
                "enum":["summary","detail"],
                "default":"summary",
                "description":"summary returns id/title/status/project only; detail returns the legacy full plan rows."
            }
        },
        "required": ["status"]
    })
}

fn schema_plan_add_task() -> Value {
    json!({
        "type":"object",
        "properties": {
            "plan_id":  {"type":"string"},
            "task_id":  {"type":"string"},
            "position": {"type":"integer","minimum":0},
            "depends_on": {"type":"array","items":{"type":"string"}}
        },
        "required":["plan_id","task_id"]
    })
}

fn schema_plan_task_ref() -> Value {
    json!({
        "type":"object",
        "properties": {
            "plan_id": {"type":"string"},
            "task_id": {"type":"string"}
        },
        "required":["plan_id","task_id"]
    })
}

fn schema_plan_reorder() -> Value {
    json!({
        "type":"object",
        "properties": {
            "plan_id": {"type":"string"},
            "order":   {"type":"array","items":{"type":"string"}}
        },
        "required":["plan_id","order"]
    })
}

fn schema_plan_next_task() -> Value {
    json!({
        "type":"object",
        "properties": {
            "id":             {"type":"string","description":"Plan id"},
            "run_id":         {"type":"string"},
            "claim_ttl_secs": {"type":"integer","minimum":0}
        },
        "required":["id","run_id"]
    })
}

fn schema_plan_drain_next() -> Value {
    json!({
        "type":"object",
        "properties": {
            "plan_id": {"type":"string","description":"Plan id"},
            "run_id": {"type":"string","description":"Optional run id; omitted creates an ephemeral id server-side."},
            "claim_ttl_secs": {"type":"integer","minimum":1,"description":"Claim TTL in seconds; defaults to 300."}
        },
        "required":["plan_id"]
    })
}

fn schema_can_start() -> Value {
    json!({
        "type":"object",
        "properties": {
            "task_id": {"type":"string","description":"Task id to check for active blockers"}
        },
        "required":["task_id"]
    })
}

fn schema_workspacegraph_context() -> Value {
    json!({
        "type":"object",
        "properties": {
            "node_id": {"type":"string","description":"Graph node id (e.g. `task:<uuid>`, `plan:<uuid>`)"},
            "limit": {"type":"integer","minimum":1,"maximum":100,"default":20}
        },
        "required":["node_id"]
    })
}

fn schema_workspacegraph_related() -> Value {
    json!({
        "type":"object",
        "properties": {
            "node_id": {"type":"string","description":"Graph node id to expand from"},
            "depth": {"type":"integer","minimum":1,"maximum":5,"default":2},
            "limit": {"type":"integer","minimum":1,"maximum":100,"default":20}
        },
        "required":["node_id"]
    })
}

fn schema_workspacegraph_search() -> Value {
    json!({
        "type":"object",
        "properties": {
            "query": {"type":"string"},
            "project_id": {
                "type":"string",
                "description":"Project id, or `all` for every project. Omitted uses the resolved repo project when unambiguous."
            },
            "scope": {
                "type":"string",
                "description":"Named daruma scope (usually repo folder name)."
            },
            "project_scope": {
                "type":"string",
                "description":"Named daruma scope (alias-safe form)."
            },
            "scope_path": {
                "type":"string",
                "description":"Filesystem path used to resolve the nearest configured daruma scope."
            },
            "limit": {"type":"integer","minimum":1,"maximum":100,"default":20}
        },
        "required":["query"]
    })
}

fn schema_workspacegraph_impact() -> Value {
    json!({
        "type":"object",
        "properties": {
            "node_id": {"type":"string","description":"Graph node id to analyze downstream impact from"},
            "limit": {"type":"integer","minimum":1,"maximum":100,"default":20}
        },
        "required":["node_id"]
    })
}

// ── Run schemas (W3.2) ───────────────────────────────────────────────────────

fn schema_run_start() -> Value {
    json!({
        "type":"object",
        "properties": {
            "plan_id":       {"type":"string"},
            "agent_id":      {"type":"string"},
            "parent_run_id": {"type":"string"}
        },
        "required":["plan_id","agent_id"]
    })
}

fn schema_run_step() -> Value {
    json!({
        "type":"object",
        "properties": {
            "run_id":  {"type":"string"},
            "task_id": {"type":"string"}
        },
        "required":["run_id","task_id"]
    })
}

fn schema_run_finish_step() -> Value {
    json!({
        "type":"object",
        "properties": {
            "run_id":  {"type":"string"},
            "task_id": {"type":"string"},
            "outcome": {
                "type":"object",
                "properties": {
                    "kind": {
                        "type":"string",
                        "enum":["done","skipped","failed","superseded"]
                    }
                },
                "required":["kind"]
            }
        },
        "required":["run_id","task_id","outcome"]
    })
}

fn schema_run_abort() -> Value {
    json!({
        "type":"object",
        "properties": {
            "run_id": {"type":"string"},
            "reason": {"type":"string"}
        },
        "required":["run_id"]
    })
}

// ── Run-note schemas (§3.8.2) ────────────────────────────────────────────────

fn schema_run_note_append() -> Value {
    json!({
        "type":"object",
        "properties": {
            "run_id": {"type":"string"},
            "body":   {"type":"string","maxLength":4096,"minLength":1}
        },
        "required":["run_id","body"]
    })
}

fn schema_run_log() -> Value {
    json!({
        "type":"object",
        "properties": {
            "run_id": {"type":"string"},
            "level": {
                "type":"string",
                "enum":["debug","info","warn","error"],
                "default":"info"
            },
            "body": {"type":"string","maxLength":4096,"minLength":1}
        },
        "required":["run_id","body"]
    })
}

fn schema_run_notes_list() -> Value {
    json!({
        "type":"object",
        "properties": {
            "run_id": {"type":"string"},
            "limit":  {"type":"integer","minimum":1,"maximum":500},
            "after":  {"type":"string","description":"Cursor: id of last note from previous page"}
        },
        "required":["run_id"]
    })
}

// ── Claim schemas (W3.2) ─────────────────────────────────────────────────────

fn schema_claim() -> Value {
    json!({
        "type":"object",
        "properties": {
            "agent_id": {"type":"string"},
            "task_id":  {"type":"string"},
            "ttl_secs": {"type":"integer","minimum":1,"maximum":86400}
        },
        "required":["agent_id","task_id"]
    })
}

fn schema_release() -> Value {
    json!({
        "type":"object",
        "properties": {
            "agent_id": {"type":"string"},
            "task_id":  {"type":"string"}
        },
        "required":["agent_id","task_id"]
    })
}

// ── Work-lease schemas (parallel-agent file coordination) ───────────────────

fn schema_work_unit_create() -> Value {
    json!({
        "type":"object",
        "properties": {
            "task_id": {"type":"string"},
            "title": {"type":"string"},
            "description": {"type":"string"},
            "stage_plan_id": {"type":"string","description":"Optional stage (plan with parent_plan_id)."},
            "priority": {"type":"string","enum":["p0","p1","p2","p3"]},
            "capability_tags": {"type":"array","items":{"type":"string"}},
            "artifact_refs": {"type":"array","items":{"type":"string"},"description":"Resource URIs leased exclusively on claim."},
            "acceptance": {"type":"array","items":{"type":"string"}}
        },
        "required":["task_id","title"]
    })
}

fn schema_work_unit_drain() -> Value {
    json!({
        "type":"object",
        "properties": {
            "task_id": {"type":"string"},
            "ttl_secs": {"type":"integer","minimum":1,"maximum":86400,"description":"Claim + lease TTL (default 300)."}
        },
        "required":["task_id"]
    })
}

fn schema_work_unit_complete() -> Value {
    json!({
        "type":"object",
        "properties": {
            "id": {"type":"string"},
            "outcome": {"type":"string"},
            "produced_artifacts": {"type":"array","items":{"type":"string"}},
            "next_suggested_units": {"type":"array","items":{"type":"string"},"description":"Follow-up unit ids the completer suggests dispatching next (advisory)."}
        },
        "required":["id"]
    })
}

fn schema_handoff_request() -> Value {
    json!({
        "type":"object",
        "properties": {
            "from_work_unit_id": {"type":"string","description":"The producing unit handing work over."},
            "to_work_unit_id":   {"type":"string","description":"The consuming unit gated on this handoff."},
            "required_artifact_ids": {"type":"array","items":{"type":"string"},"description":"Artifact URIs the consumer needs."},
            "required_state": {"type":"string","enum":["draft","reviewed","approved","implemented","verified"],"description":"State the artifacts must reach (advisory until the artifact-registry integration)."},
            "checklist": {"type":"array","items":{"type":"string"},"description":"Acceptance checklist shown to the accepting side."}
        },
        "required":["from_work_unit_id","to_work_unit_id"]
    })
}

fn schema_handoff_respond() -> Value {
    json!({
        "type":"object",
        "properties": {
            "handoff_id": {"type":"string"},
            "decision":   {"type":"string","enum":["accept","reject"]},
            "notes":      {"type":"string","description":"Optional acceptance notes."},
            "reason":     {"type":"string","description":"Required when rejecting."},
            "required_changes": {"type":"array","items":{"type":"string"},"description":"Changes required before a re-request (reject only)."}
        },
        "required":["handoff_id","decision"]
    })
}

fn schema_project_settings_update() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {"type":"string"},
            "interview": {"type":"boolean","description":"Auto-append agent activity to the Interview document."},
            "human_log": {"type":"boolean","description":"Auto-append human milestones to the Human Log document."}
        },
        "required":["project_id"]
    })
}

fn schema_artifact_register() -> Value {
    json!({
        "type": "object",
        "properties": {
            "uri":         {"type":"string","description":"Canonical resource URI — artifact://, file://, contract://, or env://."},
            "title":       {"type":"string","description":"Short human-readable name."},
            "description": {"type":"string","description":"Optional longer description."},
            "task_id":     {"type":"string","description":"Task that produces this artifact (creates a Produces edge)."},
            "project_id":  {"type":"string","description":"Project scope (creates a Contains edge)."}
        },
        "required": ["uri","title"]
    })
}

fn schema_artifact_list() -> Value {
    json!({
        "type": "object",
        "properties": {
            "project_id": {"type":"string","description":"Scope to a project."},
            "task_id":    {"type":"string","description":"Scope to a task."},
            "status":     {
                "type":"string",
                "enum":["pending","active","committed","deprecated"],
                "description":"Filter by lifecycle status."
            }
        }
    })
}

fn schema_artifact_impact() -> Value {
    json!({
        "type": "object",
        "properties": {
            "artifact_id": {"type":"string","description":"Artifact id to analyze downstream dependents from."},
            "limit":       {"type":"integer","minimum":1,"maximum":100,"default":20}
        },
        "required": ["artifact_id"]
    })
}

fn schema_workspace_resolve() -> Value {
    json!({
        "type":"object",
        "properties": {
            "scope_path": {"type":"string","description":"Filesystem root to resolve (defaults to this session's workspace key). Relative paths resolve from the workspace key."},
            "create": {"type":"boolean","description":"Create-and-bind a logical workspace + default project for an unknown root (default true)."},
            "workspace_id": {"type":"string","description":"Bind the root into this existing workspace instead of deriving one from the folder name."}
        }
    })
}

fn schema_project_move_workspace() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {"type":"string"},
            "workspace_id": {"type":"string","description":"Destination logical workspace id."},
            "root_path": {"type":"string","description":"Optional filesystem root to bind to the project."}
        },
        "required":["project_id","workspace_id"]
    })
}

fn schema_reserve_files() -> Value {
    json!({
        "type":"object",
        "properties": {
            "agent_id":   {"type":"string"},
            "task_id":    {"type":"string"},
            "project_id": {"type":"string", "description":"Scope leases to a project so identical paths in different repos don't collide."},
            "paths":      {
                "type":"array",
                "items": {"type":"string"},
                "description":"Repo-relative path globs to reserve (dirs or files; `*` matches one segment, `**` matches the rest)."
            },
            "targets":    {
                "type":"array",
                "items": {"type":"string"},
                "description":"Resource URIs to reserve: file://<glob>, artifact://<kind>/<name>, contract://<name>[@version], env://<name>. Merged with `paths`."
            },
            "mode":       {
                "type":"string",
                "enum":["exclusive","shared_read","review","intent"],
                "description":"Lease mode (default exclusive). shared_read/review coexist; intent is advisory and never blocks."
            },
            "ttl_secs":   {"type":"integer","minimum":1,"maximum":86400,"description":"Lease lifetime in seconds (default 300). Re-call to refresh."}
        },
        "required":["agent_id","task_id"]
    })
}

fn schema_release_files() -> Value {
    json!({
        "type":"object",
        "properties": {
            "agent_id": {"type":"string"},
            "task_id":  {"type":"string"}
        },
        "required":["agent_id","task_id"]
    })
}

fn schema_active_work() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {"type":"string", "description":"Optional project scope; omit to list leases across all projects."}
        }
    })
}

fn schema_ready() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {"type":"string", "description":"Project to list the ready pool for."}
        },
        "required":["project_id"]
    })
}

fn schema_ready_drain() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id":     {"type":"string", "description":"Project to claim the next ready task from."},
            "claim_ttl_secs": {"type":"integer","minimum":1,"maximum":86400,"description":"Claim lifetime in seconds (default 300)."}
        },
        "required":["project_id"]
    })
}

fn schema_doctor() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {"type":"string", "description":"Project to reconcile."}
        },
        "required":["project_id"]
    })
}

fn schema_suggest_files() -> Value {
    json!({
        "type":"object",
        "properties": {
            "task_id": {"type":"string", "description":"Task whose title/description is mined for path-like tokens."}
        },
        "required":["task_id"]
    })
}

// ── Session schemas (W3.2 / Linear B.1) ─────────────────────────────────────

fn schema_session_start() -> Value {
    json!({
        "type":"object",
        "properties": {
            "agent_id":        {
                "type":"string",
                "description":"Defaults to this MCP process agent id (see daruma_workspace_info.mcp_agent_id)."
            },
            "parent_agent_id": {"type":"string"},
            "metadata":        {
                "type":"object",
                "description":"Traceability payload. Recommended keys: client, model, chat_id, transcript_path, workspace_path. Env defaults: DARUMA_CLIENT, DARUMA_MODEL, DARUMA_CHAT_ID, DARUMA_TRANSCRIPT_PATH.",
                "properties": {
                    "client": {"type":"string", "description":"IDE client, e.g. cursor, codex, claude-code"},
                    "model": {"type":"string", "description":"Model display name or id"},
                    "chat_id": {"type":"string", "description":"Opaque conversation id in the client"},
                    "transcript_path": {"type":"string", "description":"Absolute path to chat transcript jsonl if known"},
                    "workspace_path": {"type":"string", "description":"Repo or workspace root"}
                }
            }
        },
        "required":[]
    })
}

fn schema_session_list() -> Value {
    json!({
        "type":"object",
        "properties": {
            "agent_id": {
                "type":"string",
                "description":"Defaults to this MCP process agent id."
            }
        },
        "required":[]
    })
}

fn schema_session_set_plan() -> Value {
    json!({
        "type":"object",
        "properties": {
            "id": {"type":"string","description":"Session id"},
            "steps": {
                "type":"array",
                "items": {
                    "type":"object",
                    "properties": {
                        "label":  {"type":"string"},
                        "status": {"type":"string","enum":["pending","in_progress","done","skipped"]}
                    },
                    "required":["label"]
                },
                "maxItems": 100
            }
        },
        "required":["id","steps"]
    })
}

fn schema_session_artifact() -> Value {
    json!({
        "type":"object",
        "properties": {
            "session_id": {"type":"string","description":"Agent session id"},
            "kind": {"type":"string","enum":["file","url","diff"]},
            "ref": {"type":"string","minLength":1,"description":"File path, URL, or diff reference"},
            "metadata": {"type":"object"}
        },
        "required":["session_id","kind","ref"]
    })
}

// ── Relation schemas (§3.2 W3.2) ────────────────────────────────────────────

fn schema_link() -> Value {
    json!({
        "type":"object",
        "properties": {
            "from": {"type":"string","description":"Source task id"},
            "to":   {"type":"string","description":"Target task id"},
            "kind": {
                "type":"string",
                "enum":["blocks","relates_to","duplicates"],
                "description":"Relation kind"
            },
            "client_command_id": {
                "type":"string",
                "description":"Optional idempotency key (UUID)"
            }
        },
        "required":["from","to","kind"]
    })
}

fn schema_unlink() -> Value {
    json!({
        "type":"object",
        "properties": {
            "relation_id": {"type":"string","description":"Relation id to delete"}
        },
        "required":["relation_id"]
    })
}

fn schema_relations() -> Value {
    json!({
        "type":"object",
        "properties": {
            "task_id": {"type":"string","description":"Task id to fetch relations for"}
        },
        "required":["task_id"]
    })
}

// ── Signal schemas (W3.2 / Linear B.5) ──────────────────────────────────────

fn schema_signal_send() -> Value {
    json!({
        "type":"object",
        "properties": {
            "run_id": {"type":"string"},
            "kind": {
                "type":"object",
                "description":"Signal payload — e.g. {\"type\":\"stop\"} or {\"type\":\"elicit\",\"prompt\":\"...\"}",
                "properties": {
                    "type": {"type":"string","enum":["stop","elicit","auth_required","intervention_accepted"]}
                },
                "required":["type"]
            }
        },
        "required":["run_id","kind"]
    })
}

fn schema_signal_respond() -> Value {
    json!({
        "type":"object",
        "properties": {
            "run_id": {"type":"string"},
            "choice": {"type":"string","description":"Human's response to the elicitation prompt"}
        },
        "required":["run_id","choice"]
    })
}

// ── Document schemas (PR1 §7) ────────────────────────────────────────────────

fn schema_doc_create() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id": {"type":"string"},
            "kind":       {"type":"string","enum":["interview","human_log"]},
            "title":      {"type":"string"},
            "content":    {"type":"string","description":"Initial markdown body. Defaults to empty when omitted."},
            "status":     {"type":"string","enum":["draft","active","outdated","archived"],"description":"Initial lifecycle status. Defaults to `active`."},
            "task_id":    {"type":"string","description":"Task this document is an artifact of."},
            "trigger_kind": {"type":"string","description":"What triggered the document's creation (free-form, e.g. `before_start_rule`)."},
            "consumer":   {"type":"string","description":"Who/what is expected to consume the document (free-form, e.g. `reviewer`)."}
        },
        "required":["project_id","kind","title"]
    })
}

fn schema_doc_set_status() -> Value {
    json!({
        "type":"object",
        "properties": {
            "document_id": {"type":"string"},
            "status":      {"type":"string","enum":["draft","active","outdated","archived"]}
        },
        "required":["document_id","status"]
    })
}

fn schema_doc_link_task() -> Value {
    json!({
        "type":"object",
        "properties": {
            "document_id": {"type":"string"},
            "task_id":     {"type":["string","null"],"description":"Task to bind the document to; omit or pass null to unlink."}
        },
        "required":["document_id"]
    })
}

fn schema_doc_append() -> Value {
    json!({
        "type":"object",
        "properties": {
            "document_id": {"type":"string"},
            "content":     {"type":"string","description":"Markdown chunk to append."}
        },
        "required":["document_id","content"]
    })
}

fn schema_doc_replace() -> Value {
    json!({
        "type":"object",
        "properties": {
            "document_id": {"type":"string"},
            "content":     {"type":"string","description":"Full markdown body replacing the existing content."}
        },
        "required":["document_id","content"]
    })
}

fn schema_doc_rename() -> Value {
    json!({
        "type":"object",
        "properties": {
            "document_id": {"type":"string"},
            "title":       {"type":"string"}
        },
        "required":["document_id","title"]
    })
}

fn schema_doc_list() -> Value {
    json!({
        "type":"object",
        "properties": {
            "project_id":       {
                "type":"string",
                "description":"Project id. When omitted, the resolved repo project is used only if unambiguous."
            },
            "scope": {"type":"string", "description":"Named daruma scope (usually repo folder name)."},
            "project_scope": {"type":"string", "description":"Named daruma scope (alias-safe form)."},
            "scope_path": {"type":"string", "description":"Filesystem path used to resolve the nearest configured daruma scope."},
            "kind":             {"type":"string","enum":["interview","human_log"]},
            "include_archived": {"type":"boolean","default":false}
        }
    })
}

fn schema_history_entity() -> Value {
    json!({
        "type":"object",
        "properties": {
            "entity_type": {"type":"string","enum":["task","document"]},
            "entity_id":   {"type":"string"},
            "limit":       {"type":"integer","minimum":1,"maximum":200,"default":50}
        },
        "required":["entity_type","entity_id"]
    })
}

fn schema_history_compare() -> Value {
    json!({
        "type":"object",
        "properties": {
            "entity_type": {"type":"string","enum":["task","document"]},
            "entity_id":   {"type":"string"},
            "from":        {"type":"integer","minimum":1},
            "to":          {"type":"integer","minimum":1}
        },
        "required":["entity_type","entity_id","from","to"]
    })
}

fn schema_history_latest() -> Value {
    json!({
        "type":"object",
        "properties": {
            "limit": {"type":"integer","minimum":1,"maximum":200,"default":50}
        }
    })
}

// ── helpers ──────────────────────────────────────────────────────────────────

async fn resolve_project_filter(
    client: &ApiClient,
    args: &Map<String, Value>,
    allow_all: bool,
    allow_null_inbox: bool,
    allow_scope_alias: bool,
) -> anyhow::Result<ProjectFilter> {
    if let Some(value) = args.get("project_id") {
        return match value {
            Value::Null if allow_null_inbox => Ok(ProjectFilter::None),
            Value::Null => anyhow::bail!("`project_id` cannot be null for this tool"),
            Value::String(pid) if pid == "all" && allow_all => Ok(ProjectFilter::All),
            Value::String(pid) if pid == "all" => {
                anyhow::bail!("`project_id: all` is not valid for this tool")
            }
            Value::String(pid) => Ok(ProjectFilter::Project(pid.clone())),
            other => anyhow::bail!("`project_id` must be a string or null, got {other}"),
        };
    }

    // ponytail: one GET /v1/repo-scopes per unscoped call; add a process
    // cache if the extra round-trip ever shows up in latency.
    let view = workspace::ScopeView::fetch_or_empty(client).await;

    if let Some(project_scope) = args.get("project_scope").and_then(|v| v.as_str()) {
        return resolve_named_scope(&view, project_scope);
    }
    if allow_scope_alias {
        if let Some(scope) = args.get("scope").and_then(|v| v.as_str()) {
            return resolve_named_scope(&view, scope);
        }
    }
    if let Some(scope_path) = args.get("scope_path").and_then(|v| v.as_str()) {
        return view
            .project_for_path(scope_path)?
            .map(ProjectFilter::Project)
            .ok_or_else(|| anyhow::anyhow!("no daruma scope configured for path `{scope_path}`"));
    }

    view.inferred_project().map(|p| match p {
        Some(project_id) => ProjectFilter::Project(project_id),
        None => ProjectFilter::None,
    })
}

fn resolve_named_scope(
    view: &workspace::ScopeView,
    scope: &str,
) -> anyhow::Result<ProjectFilter> {
    view.project_for_scope(scope)?
        .map(ProjectFilter::Project)
        .ok_or_else(|| anyhow::anyhow!("unknown daruma scope `{scope}`"))
}

async fn project_selection_response(
    client: &ApiClient,
    requested_status: &str,
) -> anyhow::Result<Value> {
    let projects = client.get_json("/v1/projects").await?;
    let projects = projects
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|project| {
                    json!({
                        "id": project.get("id").cloned().unwrap_or(Value::Null),
                        "title": project.get("title").cloned().unwrap_or(Value::Null),
                        "slug": project.get("slug").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(json!({
        "needs_project_selection": true,
        "reason": "No default Daruma project is resolved for this MCP workspace. To avoid a token-heavy all-project task listing, choose a project first.",
        "requested_status": requested_status,
        "projects": projects,
        "next_step": "Ask the user which project to use, then call daruma_project_use with that project_id. After that, retry daruma_list with the same status; the saved default project will be reused by later calls.",
        "next_tool": {
            "name": "daruma_project_use",
            "arguments": {
                "project_id": "<selected_project_id>"
            }
        }
    }))
}

async fn create_captured_task(
    client: &ApiClient,
    text: &str,
    args: &Map<String, Value>,
) -> anyhow::Result<Value> {
    let mut task = json!({
        "title": text,
        "status": "inbox",
        "priority": "p3"
    });
    let filter = resolve_project_filter(client, args, false, true, true).await?;
    if let Some(t) = task.as_object_mut() {
        match filter {
            ProjectFilter::Project(pid) => {
                t.insert("project_id".to_string(), Value::String(pid));
            }
            ProjectFilter::None => {}
            ProjectFilter::All => unreachable!("allow_all=false"),
        }
    }
    client
        .post_command(json!({"type":"create_task","task": task}))
        .await
}

fn view_arg(args: &Map<String, Value>, default: &str, allowed: &[&str]) -> anyhow::Result<String> {
    let view = args
        .get("view")
        .and_then(|v| v.as_str())
        .unwrap_or(default)
        .trim();
    if allowed.contains(&view) {
        Ok(view.to_string())
    } else {
        anyhow::bail!(
            "unknown view: {view:?} (expected one of: {})",
            allowed.join(", ")
        )
    }
}

fn mcp_collection_limit(args: &Map<String, Value>) -> usize {
    match args.get("limit").and_then(Value::as_u64) {
        Some(raw) => usize::try_from(raw)
            .unwrap_or(MCP_MAX_COLLECTION_LIMIT)
            .clamp(1, MCP_MAX_COLLECTION_LIMIT),
        None => MCP_DEFAULT_COLLECTION_LIMIT,
    }
}

fn mcp_page_by_id(value: Value, cursor: Option<&str>, limit: usize) -> Value {
    let Value::Array(rows) = value else {
        return value;
    };
    let total = rows.len();
    let start = match cursor {
        Some(cursor) => rows
            .iter()
            .position(|row| row.get("id").and_then(Value::as_str) == Some(cursor))
            .map(|idx| idx + 1)
            .unwrap_or(total),
        None => 0,
    };
    mcp_page_rows(
        rows,
        start,
        limit,
        |offset, page| {
            page.last()
                .and_then(|row| row.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some((offset + page.len()).to_string()))
        },
        total,
    )
}

fn mcp_page_by_offset(value: Value, cursor: Option<&str>, limit: usize) -> Value {
    let Value::Array(rows) = value else {
        return value;
    };
    let total = rows.len();
    let start = cursor
        .and_then(|cursor| cursor.parse::<usize>().ok())
        .unwrap_or(0);
    mcp_page_rows(
        rows,
        start,
        limit,
        |offset, page| Some((offset + page.len()).to_string()),
        total,
    )
}

/// Rough token estimate from a byte count. Real tokenisation varies by model;
/// `bytes / 4` is the usual order-of-magnitude heuristic and is documented as
/// approximate wherever it surfaces (see `docs/mcp/TOKEN-ECONOMY.md`).
const BYTES_PER_TOKEN_ESTIMATE: u64 = 4;

/// Serialized byte length of a JSON value — the same measure the server uses
/// for `result_bytes` telemetry (`serde_json::to_vec(...).len()`).
fn json_bytes(value: &Value) -> u64 {
    serde_json::to_vec(value).map(|b| b.len() as u64).unwrap_or(0)
}

/// A reusable handle describing content that was withheld from a response so
/// the caller can decide whether to hydrate the rest *without* a blind
/// `daruma_get` / `daruma_plan_get`. Shared by list pagination (task B) and
/// single-object bounded excerpts (task C).
#[derive(Debug, Clone, PartialEq, Serialize)]
struct TruncationMarker {
    /// How to hydrate the rest: a pagination cursor for a list, or the object
    /// id for a single-object excerpt.
    pointer: String,
    /// Serialized bytes of the withheld content.
    remaining_bytes: u64,
    /// Rough token estimate for the withheld content (`remaining_bytes / 4`).
    /// Approximate — see `docs/mcp/TOKEN-ECONOMY.md`.
    remaining_tokens_estimate: u64,
    /// One human-readable line summarising what was withheld.
    summary: String,
}

impl TruncationMarker {
    fn new(pointer: impl Into<String>, remaining_bytes: u64, summary: impl Into<String>) -> Self {
        Self {
            pointer: pointer.into(),
            remaining_bytes,
            remaining_tokens_estimate: remaining_bytes / BYTES_PER_TOKEN_ESTIMATE,
            summary: summary.into(),
        }
    }

    /// Marker for a paginated list tail: `remaining_items` more rows are
    /// reachable via `pointer` (a cursor).
    fn for_list_tail(pointer: impl Into<String>, remaining_items: usize, remaining_bytes: u64) -> Self {
        let pointer = pointer.into();
        let summary = format!(
            "{remaining_items} more item(s) available, ~{remaining_bytes} bytes (~{} tokens); paginate with the cursor",
            remaining_bytes / BYTES_PER_TOKEN_ESTIMATE
        );
        Self::new(pointer, remaining_bytes, summary)
    }

    /// Marker for a single-object excerpt: prose was trimmed to fit a token
    /// budget; re-read `pointer` (the object id) without a budget for the
    /// full object.
    fn for_object_excerpt(pointer: impl Into<String>, remaining_bytes: u64, what: &str) -> Self {
        let pointer = pointer.into();
        let summary = format!(
            "{what} trimmed to fit token budget, ~{remaining_bytes} bytes (~{} tokens) withheld; re-read id {pointer} without `max_tokens` for the full object",
            remaining_bytes / BYTES_PER_TOKEN_ESTIMATE
        );
        Self::new(pointer, remaining_bytes, summary)
    }

    fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

fn mcp_page_rows<F>(
    rows: Vec<Value>,
    start: usize,
    limit: usize,
    next_cursor: F,
    total: usize,
) -> Value
where
    F: Fn(usize, &[Value]) -> Option<String>,
{
    let mut tail = rows.into_iter().skip(start).collect::<Vec<_>>();
    let truncated = tail.len() > limit;
    // Split off the withheld remainder so its size can be measured.
    let rest = if truncated { tail.split_off(limit) } else { Vec::new() };
    let page = tail;
    let next_cursor = if truncated {
        next_cursor(start, &page)
    } else {
        None
    };
    let truncation = if truncated {
        let remaining_bytes = json_bytes(&Value::Array(rest.clone()));
        let pointer = next_cursor.clone().unwrap_or_default();
        Some(TruncationMarker::for_list_tail(pointer, rest.len(), remaining_bytes).to_value())
    } else {
        None
    };
    let returned = page.len();
    json!({
        "items": page,
        "next_cursor": next_cursor,
        "has_more": truncated,
        "truncated": truncated,
        "returned": returned,
        "total": total,
        "truncation": truncation,
    })
}

/// No-compress contract (see `docs/mcp/TOKEN-ECONOMY.md`).
///
/// Fields that any `view=summary` / excerpt / truncated response MUST keep
/// intact. These are the short structural handles a caller needs to decide
/// what to hydrate next — short ids, lifecycle status, priority, error
/// signals, and every FK-style reference already used by the per-view
/// allowlists. Only *prose* (`description`, comment `body`, `snippet`, …)
/// may ever be dropped or truncated; these never may.
///
/// `error`/`last_error` are reserved: the domain `Task`/run/plan projections
/// do not carry such a field today, but if one lands it must be exempt from
/// summarisation from day one rather than silently compressed away.
const PROTECTED_SUMMARY_FIELDS: &[&str] = &[
    "id",
    "status",
    "priority",
    "project_id",
    "task_id",
    "plan_id",
    "parent_plan_id",
    "error",
    "last_error",
];

/// Union a per-view allowlist with [`PROTECTED_SUMMARY_FIELDS`], preserving
/// the local order first (so existing output ordering is unchanged) and
/// appending any protected key not already present.
///
/// This guarantees the no-compress contract even if a future view forgets to
/// list `id`/`status`/`priority`/`*_id` explicitly. Because [`keep_keys`]
/// only copies keys that are actually present on a row, appending protected
/// keys that a given row does not have is a no-op — so behaviour for the
/// existing list/search/plan_list views is byte-for-byte identical.
fn summary_keys(local: &[&'static str]) -> Vec<&'static str> {
    let mut keys: Vec<&'static str> = local.to_vec();
    for protected in PROTECTED_SUMMARY_FIELDS {
        if !keys.contains(protected) {
            keys.push(protected);
        }
    }
    keys
}

/// [`summarize_rows`] with the no-compress contract enforced: the caller
/// supplies only the view-specific keys and the protected set is unioned in
/// automatically.
fn summarize_rows_protected(value: Value, local: &[&'static str]) -> Value {
    summarize_rows(value, &summary_keys(local))
}

fn summarize_rows(value: Value, keys: &[&str]) -> Value {
    match value {
        Value::Array(rows) => {
            Value::Array(rows.into_iter().map(|row| keep_keys(&row, keys)).collect())
        }
        Value::Object(mut obj) if obj.get("items").and_then(Value::as_array).is_some() => {
            let items = obj
                .remove("items")
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();
            obj.insert(
                "items".to_string(),
                Value::Array(items.into_iter().map(|row| keep_keys(&row, keys)).collect()),
            );
            Value::Object(obj)
        }
        other => keep_keys(&other, keys),
    }
}

fn keep_keys(value: &Value, keys: &[&str]) -> Value {
    let Some(obj) = value.as_object() else {
        return value.clone();
    };
    let mut out = Map::new();
    for key in keys {
        if let Some(v) = obj.get(*key) {
            out.insert((*key).to_string(), v.clone());
        }
    }
    Value::Object(out)
}

fn plan_progress_view(plan_resp: Value, graph: Value) -> Value {
    let plan = keep_keys(
        plan_resp.get("plan").unwrap_or(&Value::Null),
        &[
            "id",
            "title",
            "status",
            "project_id",
            "parent_plan_id",
            "updated_at",
        ],
    );
    let progress = plan_resp.get("progress").cloned().unwrap_or(Value::Null);
    let nodes = graph
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let status_by_id: HashMap<String, String> = nodes
        .iter()
        .filter_map(|node| {
            Some((
                node.get("task_id")?.as_str()?.to_string(),
                node.get("status")?.as_str()?.to_string(),
            ))
        })
        .collect();

    let mut active = Vec::new();
    let mut blocked = Vec::new();
    let mut next = Vec::new();
    for node in &nodes {
        let status = node.get("status").and_then(Value::as_str).unwrap_or("");
        if matches!(status, "done" | "cancelled") {
            continue;
        }
        let summary = keep_keys(node, &["task_id", "title", "status", "position"]);
        if matches!(status, "in_progress" | "in_review") {
            active.push(summary.clone());
        }
        if node_is_blocked(node, graph.get("edges"), &status_by_id) {
            blocked.push(summary.clone());
        } else if matches!(status, "inbox" | "todo") {
            next.push(summary);
        }
    }

    json!({
        "plan": plan,
        "progress": progress,
        "active": active.into_iter().take(5).collect::<Vec<_>>(),
        "blocked": blocked.into_iter().take(5).collect::<Vec<_>>(),
        "next": next.into_iter().take(5).collect::<Vec<_>>(),
    })
}

fn node_is_blocked(
    node: &Value,
    edges: Option<&Value>,
    status_by_id: &HashMap<String, String>,
) -> bool {
    let Some(task_id) = node.get("task_id").and_then(Value::as_str) else {
        return false;
    };
    let deps_block = node
        .get("depends_on")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|dep| status_by_id.get(dep).map(|s| s != "done").unwrap_or(true));
    let relations_block = edges
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|edge| {
            edge.get("kind").and_then(Value::as_str) == Some("blocks")
                && edge.get("to").and_then(Value::as_str) == Some(task_id)
                && edge
                    .get("from")
                    .and_then(Value::as_str)
                    .and_then(|from| status_by_id.get(from))
                    .map(|s| s != "done")
                    .unwrap_or(true)
        });
    deps_block || relations_block
}

fn required_string(args: &serde_json::Map<String, Value>, key: &str) -> anyhow::Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("`{key}` (string) is required"))
}

fn required_i64(args: &serde_json::Map<String, Value>, key: &str) -> anyhow::Result<i64> {
    args.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow::anyhow!("`{key}` is required and must be an integer"))
}

fn optional_u32(args: &serde_json::Map<String, Value>, key: &str) -> Option<u32> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// Canonical snake_case form for a `CommentKind` (§3.8.8).
///
/// Mirrors `daruma_domain::CommentKind::FromStr`: accepts the
/// snake_case canonical form (`"research"`), the PascalCase Rust
/// variant name (`"Research"`), and tolerates surrounding whitespace
/// and case. The mcp crate doesn't depend on `daruma-domain`, so
/// we inline the closed variant list here.
fn normalise_comment_kind(raw: &str) -> anyhow::Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "intent" => Ok("intent"),
        "progress" => Ok("progress"),
        "outcome" => Ok("outcome"),
        "blocker" => Ok("blocker"),
        "research" => Ok("research"),
        other => Err(anyhow::anyhow!(
            "unknown comment kind: {other:?} (expected one of: intent, progress, outcome, blocker, research)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── §3.8.8: comment kind normalisation ──────────────────────────────────

    #[test]
    fn normalise_comment_kind_accepts_snake_case() {
        for canonical in ["intent", "progress", "outcome", "blocker", "research"] {
            assert_eq!(normalise_comment_kind(canonical).unwrap(), canonical);
        }
    }

    #[test]
    fn normalise_comment_kind_accepts_pascal_case_and_uppercase() {
        // Task spec calls the tool with kind="Research".
        assert_eq!(normalise_comment_kind("Research").unwrap(), "research");
        assert_eq!(normalise_comment_kind("BLOCKER").unwrap(), "blocker");
        assert_eq!(normalise_comment_kind("  intent  ").unwrap(), "intent");
    }

    #[test]
    fn normalise_comment_kind_rejects_unknown() {
        let err = normalise_comment_kind("bogus").unwrap_err().to_string();
        assert!(err.contains("unknown comment kind"));
    }

    #[test]
    fn schema_comment_advertises_kind_field() {
        let schema = schema_comment();
        let props = schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("properties");
        assert!(props.contains_key("kind"), "schema must expose `kind`");
        // `kind` must NOT be required.
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required");
        assert!(
            !required.iter().any(|v| v.as_str() == Some("kind")),
            "`kind` must remain optional"
        );
    }

    #[test]
    fn token_safe_schemas_advertise_view_defaults() {
        for (schema, field, default) in [
            (schema_list(), "view", "summary"),
            (schema_search(), "view", "summary"),
            (schema_plan_list(), "view", "summary"),
            (schema_plan_get(), "view", "progress"),
        ] {
            assert_eq!(
                schema["properties"][field]["default"], default,
                "{field} default"
            );
        }
        for schema in [schema_list(), schema_search(), schema_plan_list()] {
            assert_eq!(schema["properties"]["limit"]["default"], 10);
            assert_eq!(schema["properties"]["limit"]["maximum"], 500);
        }
    }

    #[test]
    fn summary_rows_keep_only_whitelisted_fields() {
        let rows = json!([{
            "id": "tsk_1",
            "title": "Ship it",
            "status": "todo",
            "priority": "p1",
            "description": "large body"
        }]);
        let summary = summarize_rows(rows, &["id", "title", "status", "priority"]);
        assert_eq!(
            summary,
            json!([{"id":"tsk_1","title":"Ship it","status":"todo","priority":"p1"}])
        );
    }

    #[test]
    fn summary_rows_preserve_pagination_envelope() {
        let page = json!({
            "items": [{
                "id": "tsk_1",
                "title": "Ship it",
                "status": "todo",
                "description": "large body"
            }],
            "next_cursor": "tsk_1",
            "has_more": true
        });
        let summary = summarize_rows(page, &["id", "title", "status"]);
        assert_eq!(
            summary,
            json!({
                "items": [{"id":"tsk_1","title":"Ship it","status":"todo"}],
                "next_cursor": "tsk_1",
                "has_more": true
            })
        );
    }

    // ── Task A: no-compress contract ────────────────────────────────────────

    #[test]
    fn summary_keys_always_include_protected_fields() {
        // Even a view that only asks for `title` must retain the structural
        // handles: id/status/priority + every FK-style reference.
        let keys = summary_keys(&["title"]);
        for protected in PROTECTED_SUMMARY_FIELDS {
            assert!(
                keys.contains(protected),
                "protected field {protected:?} must be unioned in"
            );
        }
        // Local key preserved and comes first (output ordering unchanged).
        assert_eq!(keys[0], "title");
    }

    #[test]
    fn summary_keys_do_not_duplicate_already_listed_keys() {
        // list view already names id/status/priority/project_id — union must
        // not double them.
        let keys = summary_keys(&["id", "title", "status", "priority", "project_id"]);
        let id_count = keys.iter().filter(|k| **k == "id").count();
        assert_eq!(id_count, 1, "id must appear exactly once");
        let status_count = keys.iter().filter(|k| **k == "status").count();
        assert_eq!(status_count, 1, "status must appear exactly once");
    }

    #[test]
    fn protected_summary_drops_prose_but_keeps_handles() {
        // A row carrying prose + handles: a summary view that (buggily) only
        // lists `title` still must not shed id/status/priority/refs, but must
        // shed the prose `description`/`snippet`.
        let rows = json!([{
            "id": "tsk_1",
            "task_id": "tsk_1",
            "plan_id": "pln_9",
            "project_id": "prj_7",
            "status": "in_progress",
            "priority": "p0",
            "title": "Ship it",
            "description": "a very long prose body that should be dropped",
            "snippet": "prose excerpt"
        }]);
        let summary = summarize_rows_protected(rows, &["title"]);
        let row = &summary[0];
        // Protected handles survive.
        assert_eq!(row["id"], "tsk_1");
        assert_eq!(row["task_id"], "tsk_1");
        assert_eq!(row["plan_id"], "pln_9");
        assert_eq!(row["project_id"], "prj_7");
        assert_eq!(row["status"], "in_progress");
        assert_eq!(row["priority"], "p0");
        assert_eq!(row["title"], "Ship it");
        // Prose is compressed away.
        assert!(row.get("description").is_none(), "description is prose");
        assert!(row.get("snippet").is_none(), "snippet is prose");
    }

    #[test]
    fn protected_summary_preserves_existing_view_output() {
        // The three real views must be byte-for-byte identical to the old
        // hard-coded `summarize_rows` output (union is a no-op for present
        // keys, absent protected keys are skipped by keep_keys).
        let list_row = json!([{
            "id":"tsk_1","title":"T","status":"todo","priority":"p2",
            "project_id":"prj_1","updated_at":"2026-01-01","description":"drop me"
        }]);
        let via_protected = summarize_rows_protected(
            list_row.clone(),
            &["id", "title", "status", "priority", "project_id", "updated_at"],
        );
        let via_plain = summarize_rows(
            list_row,
            &["id", "title", "status", "priority", "project_id", "updated_at"],
        );
        assert_eq!(via_protected, via_plain);

        // Search rows: `snippet` (prose) must still be present here because
        // the search view explicitly lists it — protection is a floor, not a
        // ceiling.
        let search_row = json!([{
            "kind":"task","id":"tsk_1","title":"T","snippet":"hit ...",
            "task_id":"tsk_1","plan_id":"pln_1","project_id":"prj_1"
        }]);
        let searched = summarize_rows_protected(
            search_row,
            &["kind", "id", "title", "snippet", "task_id", "plan_id", "project_id"],
        );
        assert_eq!(searched[0]["snippet"], "hit ...");
        assert_eq!(searched[0]["task_id"], "tsk_1");
    }

    // ── Task B: truncation markers ──────────────────────────────────────────

    #[test]
    fn page_truncation_marker_populated_when_truncated() {
        let rows = Value::Array(
            (0..15)
                .map(|i| json!({"id": format!("tsk_{i:02}"), "title": "row", "status": "todo"}))
                .collect(),
        );
        let page = mcp_page_by_id(rows, None, 10);
        assert_eq!(page["truncated"], true);
        assert_eq!(page["returned"], 10);

        let marker = &page["truncation"];
        assert!(marker.is_object(), "truncation marker must be present");
        // pointer equals the pagination cursor — how to hydrate the rest.
        assert_eq!(marker["pointer"], page["next_cursor"]);
        assert_eq!(marker["pointer"], "tsk_09");
        // remaining_bytes is a real, non-zero measure of the withheld tail.
        let remaining_bytes = marker["remaining_bytes"].as_u64().expect("remaining_bytes u64");
        assert!(remaining_bytes > 0, "5 withheld rows must have non-zero bytes");
        // token estimate is bytes/4.
        assert_eq!(
            marker["remaining_tokens_estimate"].as_u64().unwrap(),
            remaining_bytes / 4
        );
        // human-readable, non-empty, and mentions the withheld count.
        let summary = marker["summary"].as_str().expect("summary string");
        assert!(!summary.is_empty(), "summary must be non-empty when truncated");
        assert!(summary.contains("5 more item"), "summary: {summary}");
    }

    #[test]
    fn page_has_no_truncation_marker_when_within_limit() {
        let rows = Value::Array(
            (0..3)
                .map(|i| json!({"id": format!("tsk_{i}"), "status": "todo"}))
                .collect(),
        );
        let page = mcp_page_by_id(rows, None, 10);
        assert_eq!(page["truncated"], false);
        assert!(page["truncation"].is_null(), "no marker when not truncated");
        assert!(page["next_cursor"].is_null());
    }

    #[test]
    fn truncation_marker_object_excerpt_summary_is_actionable() {
        let marker = TruncationMarker::for_object_excerpt("tsk_42", 1200, "task description");
        let v = marker.to_value();
        assert_eq!(v["pointer"], "tsk_42");
        assert_eq!(v["remaining_bytes"], 1200);
        assert_eq!(v["remaining_tokens_estimate"], 300);
        let summary = v["summary"].as_str().unwrap();
        assert!(summary.contains("tsk_42"), "summary must name the id to re-read");
        assert!(summary.contains("max_tokens"), "summary must hint how to get full");
    }

    #[test]
    fn plan_progress_view_keeps_counts_and_task_titles() {
        let plan = json!({
            "plan": {
                "id": "pln_1",
                "title": "Plan",
                "status": "active",
                "goal": "large goal",
                "success_criteria": ["large criteria"]
            },
            "progress": {"tasks_total": 3, "tasks_done": 1, "completion_pct": 33.3}
        });
        let graph = json!({
            "nodes": [
                {"task_id":"done","position":0,"title":"Done","status":"done","depends_on":[]},
                {"task_id":"active","position":1,"title":"Active","status":"in_progress","depends_on":[]},
                {"task_id":"blocked","position":2,"title":"Blocked","status":"todo","depends_on":["active"]},
                {"task_id":"next","position":3,"title":"Next","status":"todo","depends_on":["done"]}
            ],
            "edges": []
        });
        let view = plan_progress_view(plan, graph);

        assert_eq!(
            view["plan"],
            json!({"id":"pln_1","title":"Plan","status":"active"})
        );
        assert_eq!(view["progress"]["tasks_total"], 3);
        assert_eq!(view["active"][0]["title"], "Active");
        assert_eq!(view["blocked"][0]["title"], "Blocked");
        assert_eq!(view["next"][0]["title"], "Next");
        assert!(view["plan"].get("goal").is_none());
        assert!(view["plan"].get("success_criteria").is_none());
    }

    #[test]
    fn catalogue_includes_required_tools() {
        let names: Vec<&str> = tool_definitions().iter().map(|t| t.name).collect();
        assert!(
            names.len() >= 10,
            "AC-7: must expose ≥10 tools (got {})",
            names.len()
        );
        for required in [
            "daruma_subscribe_project",
            "daruma_inbox_pull",
            "daruma_comment",
            "daruma_reopen",
            "daruma_project_list",
            "daruma_project_create",
            "daruma_project_use",
            "daruma_move_project",
        ] {
            assert!(
                names.contains(&required),
                "missing required tool: {required}"
            );
        }
    }

    #[test]
    fn catalogue_includes_plan_run_tools() {
        let names: Vec<&str> = tool_definitions().iter().map(|t| t.name).collect();
        assert!(
            names.len() >= 44,
            "W3.2: catalogue must have ≥44 tools (got {})",
            names.len()
        );
        for required in [
            // Plan tools
            "daruma_plan_create",
            "daruma_plan_update",
            "daruma_plan_get",
            "daruma_plan_list",
            "daruma_plan_add_task",
            "daruma_plan_remove_task",
            "daruma_plan_reorder",
            "daruma_plan_archive",
            "daruma_plan_next_task",
            // Run tools
            "daruma_run_start",
            "daruma_run_start_step",
            "daruma_run_finish_step",
            "daruma_run_complete",
            "daruma_run_abort",
            // Claim tools
            "daruma_claim",
            "daruma_release",
            // Work-lease tools
            "daruma_reserve_files",
            "daruma_release_files",
            "daruma_active_work",
            // Project-wide ready pool
            "daruma_ready",
            "daruma_ready_drain",
            // Doctor + file suggestion
            "daruma_doctor",
            "daruma_suggest_files",
            // Session tools (Linear B.1)
            "daruma_session_start",
            "daruma_session_get",
            "daruma_session_list",
            "daruma_session_end",
            "daruma_session_set_plan",
            "daruma_session_artifact",
            "daruma_session_artifacts_list",
            // Signal tools (Linear B.5)
            "daruma_signal_send",
            "daruma_signal_respond",
            // Relation tools (§3.2 W3.2 / AC-9)
            "daruma_link",
            "daruma_unlink",
            "daruma_relations",
        ] {
            assert!(names.contains(&required), "missing W3.2 tool: {required}");
        }
    }

    #[test]
    fn catalogue_includes_document_tools() {
        let names: Vec<&str> = tool_definitions().iter().map(|t| t.name).collect();
        for required in [
            "daruma_doc_create",
            "daruma_doc_get",
            "daruma_doc_append",
            "daruma_doc_replace",
            "daruma_doc_rename",
            "daruma_doc_archive",
            "daruma_doc_list",
        ] {
            assert!(names.contains(&required), "missing PR1 tool: {required}");
        }
    }

    #[test]
    fn catalogue_includes_ai_analyze_complexity() {
        let names: Vec<&str> = tool_definitions().iter().map(|t| t.name).collect();
        assert!(
            names.contains(&"daruma_ai_analyze_complexity"),
            "§3.8.3: missing tool daruma_ai_analyze_complexity in {names:?}"
        );
    }

    #[test]
    fn schemas_are_valid_json() {
        for tool in tool_definitions() {
            let s = serde_json::to_string(&tool.input_schema).unwrap();
            let _: Value = serde_json::from_str(&s).unwrap();
        }
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;

    #[test]
    fn full_profile_is_a_superset_of_default() {
        let full: Vec<&str> = tool_definitions_for(ToolProfile::Full)
            .iter()
            .map(|t| t.name)
            .collect();
        let default: Vec<&str> = tool_definitions_for(ToolProfile::Default)
            .iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(full.len(), tool_definitions().len());
        assert!(default.len() < full.len());
        for name in &default {
            assert!(full.contains(name), "default tool {name} missing from full");
        }
    }

    #[test]
    fn default_profile_is_compact_and_workflow_first() {
        let default: Vec<&str> = tool_definitions_for(ToolProfile::Default)
            .iter()
            .map(|t| t.name)
            .collect();
        // Compact: meaningfully smaller than the full catalogue, but still a
        // complete capture→plan→execute→close workflow.
        assert!(
            default.len() <= 32,
            "default profile grew to {} tools — keep it compact (PROFILES.md \
             documents a 31-tool default; adding here needs a deliberate \
             budget decision, not a guard bump)",
            default.len()
        );
        for required in [
            "daruma_capture",
            "daruma_create",
            "daruma_list",
            "daruma_get",
            "daruma_search",
            "daruma_comment",
            "daruma_set_status",
            "daruma_complete",
            "daruma_plan_create",
            "daruma_plan_get",
            "daruma_plan_drain_next",
            "daruma_claim",
            "daruma_release",
            "daruma_run_start",
            "daruma_run_complete",
            "daruma_link",
        ] {
            assert!(default.contains(&required), "default must keep {required}");
        }
        // Advanced/destructive surfaces stay out of default.
        for excluded in [
            "daruma_delete",
            "daruma_project_delete",
            "daruma_history_rollback",
            "daruma_workspacegraph_search",
            "daruma_session_start",
        ] {
            assert!(
                !default.contains(&excluded),
                "{excluded} must not be in the default profile"
            );
        }
    }

    #[test]
    fn every_tool_has_title_metadata_and_unique_name() {
        let tools = tool_definitions();
        let mut seen = std::collections::HashSet::new();
        for t in &tools {
            assert!(!t.title.is_empty(), "{} missing title", t.name);
            assert!(!t.description.is_empty(), "{} missing description", t.name);
            assert_eq!(t.annotations.title, t.title, "{} annotation title", t.name);
            assert!(seen.insert(t.name), "duplicate tool name {}", t.name);
        }
    }

    #[test]
    fn annotations_are_coherent() {
        for t in tool_definitions() {
            if t.annotations.read_only_hint {
                assert!(
                    !t.annotations.destructive_hint,
                    "{} cannot be read-only and destructive",
                    t.name
                );
            }
        }
        let destructive: Vec<&str> = tool_definitions()
            .iter()
            .filter(|t| t.annotations.destructive_hint)
            .map(|t| t.name)
            .collect();
        for expected in [
            "daruma_delete",
            "daruma_project_delete",
            "daruma_plan_archive",
            "daruma_doc_archive",
            "daruma_unlink",
            "daruma_history_rollback",
        ] {
            assert!(
                destructive.contains(&expected),
                "{expected} must be destructive"
            );
        }
        let open_world: Vec<&str> = tool_definitions()
            .iter()
            .filter(|t| t.annotations.open_world_hint)
            .map(|t| t.name)
            .collect();
        for expected in ["daruma_ai_analyze_complexity"] {
            assert!(
                open_world.contains(&expected),
                "{expected} must be open-world"
            );
        }
    }

    #[test]
    fn serialized_tool_matches_mcp_shape() {
        let tools = tool_definitions();
        let sample = tools.iter().find(|t| t.name == "daruma_list").unwrap();
        let v = serde_json::to_value(sample).unwrap();
        assert!(v.get("inputSchema").is_some(), "inputSchema key");
        assert!(v.get("title").is_some(), "title key");
        let ann = v.get("annotations").expect("annotations key");
        for key in [
            "readOnlyHint",
            "destructiveHint",
            "idempotentHint",
            "openWorldHint",
            "title",
        ] {
            assert!(ann.get(key).is_some(), "annotations.{key}");
        }
        // Internal catalogue metadata must not leak to clients.
        assert!(v.get("domain").is_none());
        assert!(v.get("profile").is_none());
    }

    #[test]
    fn hidden_tools_are_not_callable_in_default_profile() {
        assert!(tool_hidden_in_profile(
            "daruma_delete",
            ToolProfile::Default
        ));
        assert!(!tool_hidden_in_profile("daruma_list", ToolProfile::Default));
        assert!(!tool_hidden_in_profile("daruma_delete", ToolProfile::Full));
        // Unknown names fall through to the normal unknown-tool error.
        assert!(!tool_hidden_in_profile("frobnicate", ToolProfile::Default));
    }

    #[test]
    fn profile_parse_and_env_resolution() {
        let _guard = crate::test_support::env_lock();
        assert_eq!(ToolProfile::parse("default"), Some(ToolProfile::Default));
        assert_eq!(ToolProfile::parse("FULL"), Some(ToolProfile::Full));
        assert_eq!(ToolProfile::parse("compat"), Some(ToolProfile::Full));
        assert_eq!(ToolProfile::parse("nope"), None);

        std::env::remove_var("DARUMA_MCP_PROFILE");
        assert_eq!(ToolProfile::from_env(), ToolProfile::Default);
        std::env::set_var("DARUMA_MCP_PROFILE", "full");
        assert_eq!(ToolProfile::from_env(), ToolProfile::Full);
        std::env::set_var("DARUMA_MCP_PROFILE", "garbage");
        assert_eq!(ToolProfile::from_env(), ToolProfile::Default);
        std::env::remove_var("DARUMA_MCP_PROFILE");
    }
}
