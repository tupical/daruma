//! MCP tool catalogue + dispatch.
//!
//! Every tool is a thin shim over a `taskagent-server` HTTP endpoint —
//! the inputs come in as JSON arguments from the MCP client and the
//! outputs are forwarded as JSON `content` text frames.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::client::ApiClient;
use crate::session_metadata;
use crate::workspace;

/// In-memory store of one-time confirm tokens used by `taskagent_project_delete`.
///
/// `token → (project_id, issued_at)`.  Tokens expire after [`CONFIRM_TTL`].
/// Cleared on MCP process restart — that is by design: the agent must
/// regenerate the token within the same session.
fn confirm_store() -> &'static Mutex<HashMap<String, (String, Instant)>> {
    static STORE: OnceLock<Mutex<HashMap<String, (String, Instant)>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

const CONFIRM_TTL: Duration = Duration::from_secs(300);

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
/// → `TASKAGENT_MCP_PROFILE` env → built-in `Default`. Clients that need the
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

    /// Resolve from `TASKAGENT_MCP_PROFILE`; unset or unrecognized → `Default`.
    pub fn from_env() -> Self {
        std::env::var("TASKAGENT_MCP_PROFILE")
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
    /// LLM-backed, talks to an external model, no persistent write.
    AiRead,
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
            Ann::AiRead => (true, false, false, true),
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
/// `domain` and `profile` are internal catalogue metadata (skipped in
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
}

#[allow(clippy::too_many_arguments)]
fn tool(
    name: &'static str,
    title: &'static str,
    description: &'static str,
    input_schema: Value,
    domain: ToolDomain,
    profile: ToolProfile,
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
    }
}

/// Full catalogue of tools (the `full` profile). Use
/// [`tool_definitions_for`] to get a profile-filtered surface.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    use ToolDomain as Dom;
    const D: ToolProfile = ToolProfile::Default;
    const F: ToolProfile = ToolProfile::Full;

    vec![
        // ── Tasks ─────────────────────────────────────────────────────────
        tool(
            "taskagent_create",
            "Create task",
            "Create a new task. `title` is required; everything else is optional.",
            schema_create(),
            Dom::Tasks, D, Ann::Write,
        ),
        tool(
            "taskagent_capture",
            "Capture inbox task",
            "Quick-capture a fleeting idea as an inbox task (priority p3). Uses the resolved repo project when unambiguous; pass `project_id`, `project_scope`, or `scope_path` in multi-repo parent folders. Pass `project_id: null` for a project-less inbox task.",
            schema_capture(),
            Dom::Tasks, D, Ann::Write,
        ),
        tool(
            "taskagent_capture_batch",
            "Capture multiple inbox tasks",
            "Capture multiple inbox tasks in one call. Each string becomes a separate task (priority p3).",
            schema_capture_batch(),
            Dom::Tasks, F, Ann::Write,
        ),
        tool(
            "taskagent_get",
            "Get task",
            "Fetch a single task by id. Use only when you need fields a recent list/search row does not already carry (those rows include title, status, and priority).",
            schema_with_id("id"),
            Dom::Tasks, D, Ann::Read,
        ),
        tool(
            "taskagent_update",
            "Update task",
            "Update a task's title, description, or due date. Recorded in the task event/activity log.",
            schema_update(),
            Dom::Tasks, D, Ann::WriteIdem,
        ),
        tool(
            "taskagent_list",
            "List tasks",
            "List tasks — the default tool for \"what's open / inventory\". Required `status`: a single value (`inbox`/`todo`/`in_progress`/`in_review`/`done`/`cancelled`), a comma-separated list, `active` (all non-terminal), or `all`. Avoid `status=all` unless the user explicitly asked for the archive — it can return a very large response. Optional `project_id` (`inbox` = no project, `all` = every project); when omitted, the resolved repo project is used if unambiguous, otherwise a compact project-selection response is returned.",
            schema_list(),
            Dom::Tasks, D, Ann::Read,
        ),
        tool(
            "taskagent_search",
            "Search tasks and comments",
            "Full-text lookup across tasks, comments, and plans for a named keyword. Use when the user names concrete text to find; to enumerate open work use `taskagent_list status=active` instead. Always pass `limit`.",
            schema_search(),
            Dom::Tasks, D, Ann::Read,
        ),
        tool(
            "taskagent_lesson_recall",
            "Recall lessons",
            "Recall lesson comments. Searches comments whose body starts with `lesson:`; optional `query` narrows the lesson prefix.",
            schema_lesson_recall(),
            Dom::Tasks, F, Ann::Read,
        ),
        tool(
            "taskagent_set_status",
            "Set task status",
            "Set a task's status (inbox / todo / in_progress / in_review / done / cancelled).",
            schema_set_status(),
            Dom::Tasks, D, Ann::WriteIdem,
        ),
        tool(
            "taskagent_set_priority",
            "Set task priority",
            "Set a task's priority (p0 / p1 / p2 / p3).",
            schema_set_priority(),
            Dom::Tasks, D, Ann::WriteIdem,
        ),
        tool(
            "taskagent_move_project",
            "Move task to another project",
            "Move a task to another project while preserving its id, comments, relations, and event history. Pass `project_id`, `project_scope`, or `scope_path`.",
            schema_move_project(),
            Dom::Tasks, F, Ann::WriteIdem,
        ),
        tool(
            "taskagent_complete",
            "Complete task",
            "Mark a task as completed.",
            schema_with_id("id"),
            Dom::Tasks, D, Ann::WriteIdem,
        ),
        tool(
            "taskagent_reopen",
            "Reopen task",
            "Reopen a completed task (sets status back to `todo`).",
            schema_with_id("id"),
            Dom::Tasks, D, Ann::WriteIdem,
        ),
        tool(
            "taskagent_delete",
            "Delete task",
            "Delete a task permanently.",
            schema_with_id("id"),
            Dom::Tasks, F, Ann::Destructive,
        ),
        tool(
            "taskagent_split",
            "Split task into subtasks",
            "Split a parent task into 2+ subtasks.",
            schema_split(),
            Dom::Tasks, F, Ann::Write,
        ),
        tool(
            "taskagent_bulk_set_status",
            "Bulk set task status",
            "Atomically set the same status on up to 50 tasks. Duplicate ids are deduped; fail-fast if any id is missing.",
            schema_bulk_set_status(),
            Dom::Tasks, F, Ann::WriteIdem,
        ),
        tool(
            "taskagent_comment",
            "Comment on task",
            "Add a comment to a task. Optional semantic `kind` (intent/progress/outcome/blocker/research).",
            schema_comment(),
            Dom::Tasks, D, Ann::Write,
        ),
        tool(
            "taskagent_can_start",
            "Check task readiness",
            "Check whether a task is ready to start, returning active blockers with title and status.",
            schema_can_start(),
            Dom::Tasks, D, Ann::Read,
        ),
        // ── Projects / workspace ──────────────────────────────────────────
        tool(
            "taskagent_project_list",
            "List projects",
            "List every project (id, title, description).",
            empty_schema(),
            Dom::Projects, D, Ann::Read,
        ),
        tool(
            "taskagent_project_create",
            "Create project",
            "Create a new project.",
            schema_project_create(),
            Dom::Projects, F, Ann::Write,
        ),
        tool(
            "taskagent_project_use",
            "Bind workspace to project",
            "Bind a workspace/repo scope to a taskagent project. When MCP runs in a folder containing multiple repos, pass `scope_path` so unscoped parent-folder calls remain explicit. Pass `project_id: null` to clear the selected scope.",
            schema_project_use(),
            Dom::Projects, D, Ann::WriteIdem,
        ),
        tool(
            "taskagent_project_delete",
            "Delete project",
            "Delete a project. Two-step, destructive: (1) call with only `id` to receive a one-time `confirm_token` (TTL 5 min) plus a contents summary; (2) call again with the same `id`, the issued `confirm_token`, AND `confirm` set to the project's exact title. The server still refuses unless the project has 0 tasks and 0 plans.",
            schema_project_delete(),
            Dom::Projects, F, Ann::Destructive,
        ),
        tool(
            "taskagent_workspace_info",
            "Show workspace info",
            "Show this MCP session's workspace key, inferred project, inference error, and known repo scopes.",
            empty_schema(),
            Dom::Admin, D, Ann::Read,
        ),
        tool(
            "taskagent_workspace_resolve",
            "Resolve/bind workspace for a path",
            "Resolve a filesystem root to its logical workspace + default project via the server registry. Unknown roots are created-and-bound on first call (pass `create:false` to probe only); the resolved project is persisted as this scope's default. Use when starting in a repo taskagent has never seen.",
            schema_workspace_resolve(),
            Dom::Projects, F, Ann::WriteIdem,
        ),
        tool(
            "taskagent_workspace_list",
            "List logical workspaces",
            "List logical workspaces from the server registry: id, name, bound filesystem roots, and project count.",
            empty_schema(),
            Dom::Projects, F, Ann::Read,
        ),
        tool(
            "taskagent_project_move_workspace",
            "Move project to workspace",
            "Move a project into another logical workspace (registry API), optionally binding a filesystem root to the project.",
            schema_project_move_workspace(),
            Dom::Projects, F, Ann::WriteIdem,
        ),
        tool(
            "taskagent_project_settings_get",
            "Get project settings",
            "Read per-project settings: the auto-append toggles for the Interview (AI log) and Human Log documents (both ON by default).",
            schema_with_id("project_id"),
            Dom::Projects, F, Ann::Read,
        ),
        tool(
            "taskagent_project_settings_update",
            "Update project settings",
            "Partially update per-project settings: pass `interview` and/or `human_log` booleans to toggle auto-append into the corresponding log document.",
            schema_project_settings_update(),
            Dom::Projects, F, Ann::WriteIdem,
        ),
        // ── AI tools ──────────────────────────────────────────────────────
        tool(
            "taskagent_ai_parse",
            "AI: parse text into command",
            "Have the AI parse free-form text into a Command (returned as JSON; nothing is dispatched).",
            schema_ai_parse(),
            Dom::Ai, F, Ann::AiRead,
        ),
        tool(
            "taskagent_ai_decompose",
            "AI: decompose task",
            "Have the AI decompose a task into subtasks. Optional `hint` (typically an `expansion_hint` from `taskagent_ai_analyze_complexity`) is appended to the prompt as guidance.",
            schema_ai_decompose(),
            Dom::Ai, F, Ann::AiWrite,
        ),
        tool(
            "taskagent_ai_analyze_complexity",
            "AI: analyze plan complexity",
            "Estimate decomposition complexity for every task in a plan in one batch LLM call. Upserts the `task_complexity_hints` projection (per-task score 1-10, recommended_subtasks, expansion_hint, reasoning). Feed `expansion_hint` into `taskagent_ai_decompose { hint }` to chain analyze → decompose.",
            schema_ai_analyze_complexity(),
            Dom::Ai, F, Ann::AiWrite,
        ),
        tool(
            "taskagent_ai_scope",
            "AI: rescope task",
            "Rewrite a task's title + description at a broader (`up`) or narrower (`down`) scope. Returns the proposed `Command::UpdateTask` JSON; the caller decides whether to dispatch.",
            schema_ai_scope(),
            Dom::Ai, F, Ann::AiRead,
        ),
        tool(
            "taskagent_research",
            "AI: research query",
            "Run a free-form research query against the AI provider, optionally grounded in the bodies of existing tasks (`context_task_ids`). When `save_to_task_id` is set, the answer is persisted as a Research comment on that task.",
            schema_ai_research(),
            Dom::Ai, F, Ann::AiWrite,
        ),
        // ── Events / health ───────────────────────────────────────────────
        tool(
            "taskagent_inbox_pull",
            "Pull agent inbox",
            "Poll a single agent's inbox; optionally long-poll up to 60 s.",
            schema_inbox_pull(),
            Dom::Coordination, F, Ann::Write,
        ),
        tool(
            "taskagent_subscribe_project",
            "Snapshot project events",
            "One-shot snapshot of events for a project (the streaming form lives on /v1/ws).",
            schema_subscribe_project(),
            Dom::Events, F, Ann::Read,
        ),
        tool(
            "taskagent_events_since",
            "Load events since seq",
            "Load events with `seq > since`, capped at `limit` (default 100).",
            schema_events_since(),
            Dom::Events, F, Ann::Read,
        ),
        tool(
            "taskagent_healthz",
            "Server health check",
            "Server health check — no auth required.",
            empty_schema(),
            Dom::Admin, D, Ann::Read,
        ),
        // ── Plans ─────────────────────────────────────────────────────────
        tool(
            "taskagent_plan_create",
            "Create plan",
            "Create a new execution plan for a project.",
            schema_plan_create(),
            Dom::Plans, D, Ann::Write,
        ),
        tool(
            "taskagent_plan_update",
            "Update plan",
            "Update a plan's title, description, goal, success criteria, or parent. Pass null for parent_plan_id to unparent (move to root); omit the field to leave the parent unchanged.",
            schema_plan_update(),
            Dom::Plans, F, Ann::WriteIdem,
        ),
        tool(
            "taskagent_plan_get",
            "Get plan",
            "Fetch a plan by id, including progress metrics — the cheap way to summarize one plan's status (prefer this over enumerating completed plans or tasks).",
            schema_with_id("id"),
            Dom::Plans, D, Ann::Read,
        ),
        tool(
            "taskagent_plan_list",
            "List plans",
            "List plans. Required `status`: `draft`/`active`/`completed`/`abandoned`, a comma-separated list, or `all`. Prefer `draft,active`; completed plans carry their full goal + success criteria and are token-heavy — summarize a single plan with `taskagent_plan_get` instead of enumerating. `project_id` uses the resolved repo project when unambiguous; pass `all` to query across projects.",
            schema_plan_list(),
            Dom::Plans, D, Ann::Read,
        ),
        tool(
            "taskagent_plan_add_task",
            "Attach task to plan",
            "Attach a task to a plan at an optional position with optional dependencies.",
            schema_plan_add_task(),
            Dom::Plans, D, Ann::Write,
        ),
        tool(
            "taskagent_plan_remove_task",
            "Detach task from plan",
            "Detach a task from a plan. Aborts any in-progress step atomically.",
            schema_plan_task_ref(),
            Dom::Plans, F, Ann::Write,
        ),
        tool(
            "taskagent_plan_reorder",
            "Reorder plan tasks",
            "Replace the full task order within a plan.",
            schema_plan_reorder(),
            Dom::Plans, F, Ann::WriteIdem,
        ),
        tool(
            "taskagent_plan_archive",
            "Archive plan",
            "Archive a plan and atomically abort all active runs.",
            schema_with_id("id"),
            Dom::Plans, F, Ann::Destructive,
        ),
        tool(
            "taskagent_plan_set_status",
            "Set plan status",
            "Transition a plan into a different lifecycle state (draft, active, completed, abandoned). Emits PlanStatusChanged.",
            schema_plan_set_status(),
            Dom::Plans, D, Ann::WriteIdem,
        ),
        tool(
            "taskagent_plan_next_task",
            "Peek next eligible plan task",
            "Return the first eligible task in a plan for a given run, respecting dependencies. May acquire a claim when `claim_ttl_secs` is set — prefer `taskagent_plan_drain_next` for parallel agents.",
            schema_plan_next_task(),
            Dom::Plans, F, Ann::Write,
        ),
        tool(
            "taskagent_plan_progress",
            "Plan progress snapshot",
            "Executor snapshot for a plan: task counts by status plus the next ready task id (when the plan is Active).",
            schema_with_id("plan_id"),
            Dom::Plans, D, Ann::Read,
        ),
        tool(
            "taskagent_plan_drain_next",
            "Claim next plan task",
            "Atomically resolve the next eligible plan task and acquire an exclusive claim for this session's agent. Concurrent callers each get a distinct task; returns null when no unclaimed ready task remains. Re-call in a loop to drain a plan across many agents.",
            schema_plan_drain_next(),
            Dom::Plans, D, Ann::Write,
        ),
        tool(
            "taskagent_plan_graph",
            "Read plan DAG",
            "Read a plan's execution DAG: task nodes plus depends_on and blocks edges.",
            schema_with_plan_id(),
            Dom::Plans, F, Ann::Read,
        ),
        tool(
            "taskagent_plan_fanout",
            "Plan execution waves",
            "Return parallel execution waves for a plan, respecting depends_on and active Blocks relations.",
            schema_with_plan_id(),
            Dom::Plans, F, Ann::Read,
        ),
        tool(
            "taskagent_bulk_attach_to_plan",
            "Bulk attach tasks to plan",
            "Atomically attach up to 50 tasks to a single plan. Already-attached tasks are skipped (idempotent); fail-fast if any task or the plan is missing.",
            schema_bulk_attach_to_plan(),
            Dom::Plans, F, Ann::WriteIdem,
        ),
        // ── WorkspaceGraph ────────────────────────────────────────────────
        tool(
            "taskagent_workspacegraph_status",
            "WorkspaceGraph index health",
            "WorkspaceGraph index health: schema version, node/edge counts, event lag, and last error.",
            empty_schema(),
            Dom::WorkspaceGraph, F, Ann::Read,
        ),
        tool(
            "taskagent_workspacegraph_context",
            "Graph node neighborhood",
            "Immediate neighborhood of a graph node (incoming/outgoing edges plus ranked neighbors).",
            schema_workspacegraph_context(),
            Dom::WorkspaceGraph, F, Ann::Read,
        ),
        tool(
            "taskagent_workspacegraph_related",
            "Graph related nodes",
            "Breadth-first related nodes around a graph node, capped by depth and limit.",
            schema_workspacegraph_related(),
            Dom::WorkspaceGraph, F, Ann::Read,
        ),
        tool(
            "taskagent_workspacegraph_search",
            "Search WorkspaceGraph nodes",
            "Full-text search over WorkspaceGraph nodes — for finding a node whose graph neighborhood you then explore. Not for listing open work (use `taskagent_list status=active`).",
            schema_workspacegraph_search(),
            Dom::WorkspaceGraph, F, Ann::Read,
        ),
        tool(
            "taskagent_workspacegraph_impact",
            "Graph impact analysis",
            "Downstream tasks and plans affected through Blocks, PlanContains, and ownership edges.",
            schema_workspacegraph_impact(),
            Dom::WorkspaceGraph, F, Ann::Read,
        ),
        // ── Runs ──────────────────────────────────────────────────────────
        tool(
            "taskagent_run_start",
            "Start run",
            "Start a new agent run of a plan.",
            schema_run_start(),
            Dom::Runs, D, Ann::Write,
        ),
        tool(
            "taskagent_run_start_step",
            "Start run step",
            "Mark the beginning of a task step within a run.",
            schema_run_step(),
            Dom::Runs, F, Ann::Write,
        ),
        tool(
            "taskagent_run_finish_step",
            "Finish run step",
            "Mark the completion of a task step with an outcome.",
            schema_run_finish_step(),
            Dom::Runs, F, Ann::Write,
        ),
        tool(
            "taskagent_run_complete",
            "Complete run",
            "Terminate a run successfully.",
            schema_with_id("run_id"),
            Dom::Runs, D, Ann::WriteIdem,
        ),
        tool(
            "taskagent_run_abort",
            "Abort run",
            "Abort a run with a reason (e.g. plan archived or explicit stop).",
            schema_run_abort(),
            Dom::Runs, D, Ann::WriteIdem,
        ),
        tool(
            "taskagent_run_note_append",
            "Append run note",
            "Append a free-form journal note to an active run. The actor is taken from the MCP session token; body is required (≤ 4 KiB).",
            schema_run_note_append(),
            Dom::Runs, D, Ann::Write,
        ),
        tool(
            "taskagent_run_log",
            "Append run log entry",
            "Append a leveled progress log entry to an active run. Uses the run notes stream with body formatted as `[level] message`.",
            schema_run_log(),
            Dom::Runs, F, Ann::Write,
        ),
        tool(
            "taskagent_run_notes_list",
            "List run notes",
            "List journal notes for a run in chronological order. Optional `limit` (default 50, max 500) and `after` (cursor = id of last note from previous page).",
            schema_run_notes_list(),
            Dom::Runs, F, Ann::Read,
        ),
        // ── Claims & leases (parallel-agent coordination) ─────────────────
        tool(
            "taskagent_claim",
            "Claim task",
            "Acquire an optimistic claim on a task for a given TTL in seconds.",
            schema_claim(),
            Dom::Coordination, D, Ann::Write,
        ),
        tool(
            "taskagent_release",
            "Release task claim",
            "Release a previously-acquired task claim.",
            schema_release(),
            Dom::Coordination, D, Ann::WriteIdem,
        ),
        tool(
            "taskagent_reserve_files",
            "Reserve file paths",
            "Reserve resources for a task so parallel agents don't collide. Pass repo-relative `paths` (globs) and/or `targets` URIs (file://, artifact://, contract://, env://) plus an optional `mode` (exclusive default; shared_read/review coexist; intent is advisory). Returns `reserved:true` with leases carrying `fencing_token`, or `reserved:false` with `conflict_path` + `holder` — then take a different task. Re-reserving extends the TTL; leases auto-release when the task closes or the TTL lapses.",
            schema_reserve_files(),
            Dom::Coordination, F, Ann::Write,
        ),
        tool(
            "taskagent_release_files",
            "Release file leases",
            "Release all file/path leases held by an agent for a task. Usually automatic on task completion; call explicitly to free files early.",
            schema_release_files(),
            Dom::Coordination, F, Ann::WriteIdem,
        ),
        tool(
            "taskagent_active_work",
            "List active file leases",
            "List the active work backlog: live file/path leases (who is touching which files) for a project. Use before reserving to see contended areas. Pass `project_id` to scope; omit for all.",
            schema_active_work(),
            Dom::Coordination, F, Ann::Read,
        ),
        tool(
            "taskagent_ready",
            "List project ready pool",
            "List the project-wide ready pool: tasks across ALL active plans whose dependencies are satisfied and that no other agent holds. The read-only view behind `taskagent_ready_drain`.",
            schema_ready(),
            Dom::Coordination, F, Ann::Read,
        ),
        tool(
            "taskagent_ready_drain",
            "Claim next ready task (project-wide)",
            "Atomically claim the next ready task across the project's active plans. Concurrent callers each get a distinct task; sets it in_progress. Returns null when nothing is ready — loop until null.",
            schema_ready_drain(),
            Dom::Coordination, F, Ann::Write,
        ),
        tool(
            "taskagent_doctor",
            "Reconcile stuck parallel work",
            "Reconcile parallel-agent state for a project: reports tasks stuck `in_progress` with no live claim (an agent likely crashed and its claim TTL lapsed). These are reclaimable — reopen or re-drain them.",
            schema_doctor(),
            Dom::Coordination, F, Ann::Read,
        ),
        tool(
            "taskagent_suggest_files",
            "Suggest paths to reserve",
            "Suggest path globs to reserve for a task by extracting path-like tokens from its title/description. Use to seed `taskagent_reserve_files` at claim time. Heuristic only — review before reserving.",
            schema_suggest_files(),
            Dom::Coordination, F, Ann::Read,
        ),
        tool(
            "taskagent_work_unit_create",
            "Create work unit",
            "Create a work unit under a task — the minimal dispatchable unit for multi-agent work on one task. Declare `artifact_refs` (file://, artifact://, contract://, env://) so the dispatcher can lease them on claim. Simple tasks don't need work units.",
            schema_work_unit_create(),
            Dom::Coordination, F, Ann::Write,
        ),
        tool(
            "taskagent_work_unit_list",
            "List task work units",
            "List all work units under a task (full decomposition state, including done/cancelled).",
            schema_with_id("task_id"),
            Dom::Coordination, F, Ann::Read,
        ),
        tool(
            "taskagent_work_unit_drain_next",
            "Claim next work unit",
            "Atomically claim the next dispatchable work unit under a task and acquire its declared exclusive resource leases. Concurrent callers each get a distinct unit. Returns a briefing {work_unit, leases (with fencing_token), acceptance}; null when nothing is dispatchable; lease_conflict (claim reverted) when the unit's resources are held elsewhere.",
            schema_work_unit_drain(),
            Dom::Coordination, F, Ann::Write,
        ),
        tool(
            "taskagent_work_unit_complete",
            "Complete work unit",
            "Mark a work unit done with an outcome and the produced artifact URIs (mineable payload). Releases the holder claim.",
            schema_work_unit_complete(),
            Dom::Coordination, F, Ann::WriteIdem,
        ),
        tool(
            "taskagent_work_unit_release",
            "Release work unit claim",
            "Release a claimed work unit back to the dispatch pool (status returns to ready).",
            schema_with_id("id"),
            Dom::Coordination, F, Ann::WriteIdem,
        ),
        // ── Sessions ──────────────────────────────────────────────────────
        tool(
            "taskagent_session_start",
            "Start agent session",
            "Start a new agent session. Pass `metadata` with client/model/chat_id/transcript_path so work can be traced back to the IDE chat. `agent_id` defaults to this MCP process id.",
            schema_session_start(),
            Dom::Sessions, F, Ann::Write,
        ),
        tool(
            "taskagent_session_get",
            "Get agent session",
            "Fetch an agent session by id (includes metadata: client, model, chat_id, transcript_path).",
            schema_with_id("id"),
            Dom::Sessions, F, Ann::Read,
        ),
        tool(
            "taskagent_session_list",
            "List agent sessions",
            "List agent sessions for an agent id (defaults to this MCP process).",
            schema_session_list(),
            Dom::Sessions, F, Ann::Read,
        ),
        tool(
            "taskagent_session_end",
            "End agent session",
            "End an agent session.",
            schema_with_id("id"),
            Dom::Sessions, F, Ann::WriteIdem,
        ),
        tool(
            "taskagent_session_set_plan",
            "Set session plan steps",
            "Replace the session's plan-steps list (max 100 steps).",
            schema_session_set_plan(),
            Dom::Sessions, F, Ann::WriteIdem,
        ),
        tool(
            "taskagent_session_artifact",
            "Attach session artifact",
            "Attach a file/url/diff artifact reference to an agent session.",
            schema_session_artifact(),
            Dom::Sessions, F, Ann::Write,
        ),
        tool(
            "taskagent_session_artifacts_list",
            "List session artifacts",
            "List artifact references attached to an agent session.",
            schema_with_id("id"),
            Dom::Sessions, F, Ann::Read,
        ),
        // ── Signals ───────────────────────────────────────────────────────
        tool(
            "taskagent_signal_send",
            "Send run signal",
            "Send a typed signal to a run (stop / elicit / auth_required).",
            schema_signal_send(),
            Dom::Signals, F, Ann::Write,
        ),
        tool(
            "taskagent_signal_respond",
            "Respond to run signal",
            "Human responds to an elicitation request on a run.",
            schema_signal_respond(),
            Dom::Signals, F, Ann::Write,
        ),
        // ── Relations ─────────────────────────────────────────────────────
        tool(
            "taskagent_link",
            "Link tasks",
            "Create a typed relation (blocks / relates_to / duplicates) between two tasks. Idempotent via `client_command_id`.",
            schema_link(),
            Dom::Relations, D, Ann::WriteIdem,
        ),
        tool(
            "taskagent_unlink",
            "Delete task relation",
            "Delete a relation by its id.",
            schema_unlink(),
            Dom::Relations, F, Ann::Destructive,
        ),
        tool(
            "taskagent_relations",
            "Read task relations",
            "Read 5-group relations projection for a task (blocks, blocked_by, relates_to, duplicates, duplicated_by).",
            schema_relations(),
            Dom::Relations, D, Ann::Read,
        ),
        // ── Documents ─────────────────────────────────────────────────────
        tool(
            "taskagent_doc_create",
            "Create document",
            "Create a markdown document for a project. `kind` is `interview` or `human_log`; multiple docs of the same kind are allowed.",
            schema_doc_create(),
            Dom::Documents, F, Ann::Write,
        ),
        tool(
            "taskagent_doc_get",
            "Get document",
            "Fetch a document by id, including its full markdown body.",
            schema_with_id("document_id"),
            Dom::Documents, F, Ann::Read,
        ),
        tool(
            "taskagent_doc_append",
            "Append to document",
            "Append markdown to a document. A blank-line separator is inserted by the server when the existing body is non-empty.",
            schema_doc_append(),
            Dom::Documents, F, Ann::Write,
        ),
        tool(
            "taskagent_doc_replace",
            "Replace document body",
            "Replace a document's entire markdown body.",
            schema_doc_replace(),
            Dom::Documents, F, Ann::WriteIdem,
        ),
        tool(
            "taskagent_doc_rename",
            "Rename document",
            "Rename a document (title only; body is unchanged).",
            schema_doc_rename(),
            Dom::Documents, F, Ann::WriteIdem,
        ),
        tool(
            "taskagent_doc_archive",
            "Archive document",
            "Soft-archive a document. It remains queryable via `taskagent_doc_list` when `include_archived=true`.",
            schema_with_id("document_id"),
            Dom::Documents, F, Ann::Destructive,
        ),
        tool(
            "taskagent_doc_list",
            "List documents",
            "List documents for a project. `project_id` uses the resolved repo project when unambiguous; multi-repo parent folders require `project_id`, `project_scope`, or `scope_path`. Optional `kind` filter; archived docs are hidden unless `include_archived=true`.",
            schema_doc_list(),
            Dom::Documents, F, Ann::Read,
        ),
        // ── Version history ───────────────────────────────────────────────
        tool(
            "taskagent_history_list",
            "List version history",
            "List immutable version records for one task or document, newest first.",
            schema_history_entity(),
            Dom::History, F, Ann::Read,
        ),
        tool(
            "taskagent_history_get",
            "Get version record",
            "Fetch one immutable version record by version id.",
            schema_with_id("version_id"),
            Dom::History, F, Ann::Read,
        ),
        tool(
            "taskagent_history_compare",
            "Compare versions",
            "Compare two version numbers for the same task or document.",
            schema_history_compare(),
            Dom::History, F, Ann::Read,
        ),
        tool(
            "taskagent_history_latest",
            "List latest versions",
            "List latest task/document version records visible to this token.",
            schema_history_latest(),
            Dom::History, F, Ann::Read,
        ),
        tool(
            "taskagent_history_summary",
            "Version summary timeline",
            "Return a compact agent-readable summary timeline for one task or document.",
            schema_history_entity(),
            Dom::History, F, Ann::Read,
        ),
        tool(
            "taskagent_history_rollback",
            "Rollback to version",
            "Restore a task or document to a selected immutable version by creating a new rollback version.",
            schema_with_id("version_id"),
            Dom::History, F, Ann::Destructive,
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
             restart the MCP server with TASKAGENT_MCP_PROFILE=full \
             (or `taskagent mcp --profile full`) to enable the full catalogue",
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
        "taskagent_create" => {
            let mut task = args.get("task").cloned().unwrap_or_else(|| json!({}));
            // Inject the workspace default project if the task didn't
            // specify one explicitly. Use `"project_id": null` in the
            // arguments to opt out and create an inbox-only task.
            if let Some(t) = task.as_object_mut() {
                if !t.contains_key("project_id") {
                    match resolve_project_filter(&args, false, true, true)? {
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
        "taskagent_capture" => {
            let text = required_string(&args, "text")?;
            create_captured_task(client, &text, &args).await
        }
        "taskagent_capture_batch" => {
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
        "taskagent_get" => {
            let id = required_string(&args, "id")?;
            client.get_json(&format!("/v1/tasks/{id}")).await
        }
        "taskagent_update" => {
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
        "taskagent_list" => {
            let status = required_string(&args, "status")?;
            let mut params: Vec<(&str, String)> = vec![("status", urlencode(status.trim()))];
            match resolve_project_filter(&args, true, false, true)? {
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
            client.get_json(&format!("/v1/tasks?{qs}")).await
        }
        "taskagent_search" => {
            let query = required_string(&args, "query")?;
            let scope = args.get("scope").and_then(|v| v.as_str());
            let limit = args.get("limit").and_then(|v| v.as_u64());
            let mut params: Vec<(&str, String)> = vec![("query", urlencode(&query))];
            if let Some(s) = scope {
                let s = s.trim();
                if !s.is_empty() {
                    params.push(("scope", urlencode(s)));
                }
            }
            match resolve_project_filter(&args, true, false, false)? {
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
        "taskagent_lesson_recall" => {
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
            match resolve_project_filter(&args, true, false, false)? {
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
        "taskagent_project_list" => client.get_json("/v1/projects").await,
        "taskagent_project_create" => {
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
        "taskagent_project_delete" => {
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
        "taskagent_project_use" => {
            let ws = workspace::global()
                .ok_or_else(|| anyhow::anyhow!("workspace state not initialised"))?;
            let scope_path = args.get("scope_path").and_then(|v| v.as_str());
            match args.get("project_id") {
                Some(v) if v.is_null() => {
                    let scope = ws.set_default_project("", scope_path)?;
                    Ok(json!({"workspace": ws.key(), "scope": scope, "project_id": Value::Null}))
                }
                Some(v) => {
                    let pid = v
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("`project_id` must be a string or null"))?;
                    let scope = ws.set_default_project(pid, scope_path)?;
                    Ok(json!({"workspace": ws.key(), "scope": scope, "project_id": pid}))
                }
                None => anyhow::bail!("`project_id` is required (use null to clear)"),
            }
        }
        "taskagent_workspace_info" => {
            let ws = workspace::global();
            let inferred = ws.map(|w| w.inferred_project());
            let (inferred_project, inferred_project_error) = match inferred {
                Some(Ok(project_id)) => (project_id, None),
                Some(Err(err)) => (None, Some(err.to_string())),
                None => (None, None),
            };
            Ok(json!({
                "workspace": ws.map(|w| w.key().to_string()),
                "mcp_agent_id": client.agent_id(),
                "default_project": inferred_project.clone(),
                "inferred_project": inferred_project,
                "inferred_project_error": inferred_project_error,
                "scopes": ws.map(|w| {
                    w.scopes()
                        .into_iter()
                        .map(|(scope, project_id)| json!({
                            "scope": scope,
                            "name": std::path::Path::new(&scope)
                                .file_name()
                                .and_then(|s| s.to_str())
                                .unwrap_or(&scope),
                            "project_id": project_id,
                        }))
                        .collect::<Vec<_>>()
                }).unwrap_or_default(),
            }))
        }
        "taskagent_set_status" => {
            let id = required_string(&args, "id")?;
            let status = required_string(&args, "status")?;
            let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
            client
                .post_command(
                    json!({"type":"set_status","id": id, "status": status, "force": force}),
                )
                .await
        }
        "taskagent_set_priority" => {
            let id = required_string(&args, "id")?;
            let priority = required_string(&args, "priority")?;
            client
                .post_command(json!({"type":"set_priority","id": id, "priority": priority}))
                .await
        }
        "taskagent_move_project" => {
            let id = required_string(&args, "id")?;
            let project_id = match resolve_project_filter(&args, false, false, true)? {
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
        "taskagent_complete" => {
            let id = required_string(&args, "id")?;
            client
                .post_command(json!({"type":"complete_task","id": id}))
                .await
        }
        "taskagent_delete" => {
            let id = required_string(&args, "id")?;
            client
                .post_command(json!({"type":"delete_task","id": id}))
                .await
        }
        "taskagent_split" => {
            let parent = required_string(&args, "parent")?;
            let subtasks = args
                .get("subtasks")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("`subtasks` (array) is required"))?;
            client
                .post_command(json!({"type":"split_task","parent": parent, "subtasks": subtasks}))
                .await
        }
        "taskagent_bulk_set_status" => {
            let ids = args
                .get("ids")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("`ids` (array of task ids) is required"))?;
            let status = required_string(&args, "status")?;
            client
                .post_command(json!({"type":"bulk_set_status","ids": ids, "status": status}))
                .await
        }
        "taskagent_bulk_attach_to_plan" => {
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
        "taskagent_ai_parse" => {
            let input = required_string(&args, "input")?;
            let mut body = json!({"input": input});
            // §3.8.13: forward use_research_provider transparently.
            if let Some(flag) = args.get("use_research_provider").and_then(|v| v.as_bool()) {
                body["use_research_provider"] = json!(flag);
            }
            client.post_json("/v1/ai/parse", body).await
        }
        "taskagent_ai_decompose" => {
            let task_id = required_string(&args, "task_id")?;
            let mut body = json!({});
            if let Some(hint) = args.get("hint").and_then(|v| v.as_str()) {
                body["hint"] = json!(hint);
            }
            if let Some(flag) = args.get("use_research_provider").and_then(|v| v.as_bool()) {
                body["use_research_provider"] = json!(flag);
            }
            client
                .post_json(&format!("/v1/ai/decompose/{task_id}"), body)
                .await
        }
        "taskagent_ai_analyze_complexity" => {
            let plan_id = required_string(&args, "plan_id")?;
            let mut body = json!({});
            if let Some(flag) = args.get("use_research_provider").and_then(|v| v.as_bool()) {
                body["use_research_provider"] = json!(flag);
            }
            client
                .post_json(&format!("/v1/ai/analyze-complexity/{plan_id}"), body)
                .await
        }
        "taskagent_ai_scope" => {
            let task_id = required_string(&args, "task_id")?;
            let direction = required_string(&args, "direction")?;
            let mut body = json!({"direction": direction});
            if let Some(s) = args.get("strength").and_then(|v| v.as_str()) {
                body["strength"] = json!(s);
            }
            if let Some(flag) = args.get("use_research_provider").and_then(|v| v.as_bool()) {
                body["use_research_provider"] = json!(flag);
            }
            client
                .post_json(&format!("/v1/ai/scope/{task_id}"), body)
                .await
        }
        "taskagent_research" => {
            let query = required_string(&args, "query")?;
            let mut body = json!({"query": query});
            if let Some(ids) = args.get("context_task_ids").and_then(|v| v.as_array()) {
                body["context_task_ids"] =
                    json!(ids.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>());
            }
            if let Some(save) = args.get("save_to_task_id").and_then(|v| v.as_str()) {
                body["save_to_task_id"] = json!(save);
            }
            if let Some(flag) = args.get("use_research_provider").and_then(|v| v.as_bool()) {
                body["use_research_provider"] = json!(flag);
            }
            client.post_json("/v1/ai/research", body).await
        }
        "taskagent_comment" => {
            let task_id = required_string(&args, "task_id")?;
            let body_text = required_string(&args, "body")?;
            // §3.8.8: optional semantic classification. We validate locally
            // against the canonical set so MCP callers get an immediate
            // error rather than a server-side 400. The authoritative parser
            // lives in `taskagent_domain::CommentKind::FromStr`, mirrored
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
        "taskagent_reopen" => {
            let id = required_string(&args, "id")?;
            client
                .post_command(json!({"type":"set_status","id": id, "status": "todo"}))
                .await
        }
        "taskagent_inbox_pull" => {
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
        "taskagent_subscribe_project" => {
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
        "taskagent_events_since" => {
            let since = args.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(100);
            client
                .get_json(&format!("/v1/events?since={since}&limit={limit}"))
                .await
        }
        "taskagent_healthz" => client.get_json("/v1/healthz").await,

        // ── Plan tools (W3.2) ─────────────────────────────────────────────
        "taskagent_plan_create" => {
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
        "taskagent_plan_update" => {
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
        "taskagent_plan_get" => {
            let id = required_string(&args, "id")?;
            client.get_json(&format!("/v1/plans/{id}")).await
        }
        "taskagent_plan_list" => {
            let status = required_string(&args, "status")?;
            let mut params: Vec<(&str, String)> = vec![("status", urlencode(status.trim()))];
            match resolve_project_filter(&args, true, false, true)? {
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
            client.get_json(&format!("/v1/plans?{qs}")).await
        }
        "taskagent_plan_add_task" => {
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
        "taskagent_plan_remove_task" => {
            let plan_id = required_string(&args, "plan_id")?;
            let task_id = required_string(&args, "task_id")?;
            client
                .delete_json(&format!("/v1/plans/{plan_id}/tasks/{task_id}"))
                .await
        }
        "taskagent_plan_reorder" => {
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
        "taskagent_plan_archive" => {
            let id = required_string(&args, "id")?;
            client
                .post_json(&format!("/v1/plans/{id}/archive"), json!({}))
                .await
        }
        "taskagent_plan_set_status" => {
            let id = required_string(&args, "plan_id")?;
            let status = required_string(&args, "status")?;
            client
                .post_json(
                    &format!("/v1/plans/{id}/status"),
                    json!({ "status": status }),
                )
                .await
        }
        "taskagent_plan_next_task" => {
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
        "taskagent_plan_progress" => {
            let plan_id = required_string(&args, "plan_id")?;
            client
                .get_json(&format!("/v1/plans/{plan_id}/progress"))
                .await
        }
        "taskagent_plan_drain_next" => {
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
        "taskagent_plan_graph" => {
            let plan_id = required_string(&args, "plan_id")?;
            client.get_json(&format!("/v1/plans/{plan_id}/graph")).await
        }
        "taskagent_plan_fanout" => {
            let plan_id = required_string(&args, "plan_id")?;
            client
                .get_json(&format!("/v1/plans/{plan_id}/fanout"))
                .await
        }
        "taskagent_can_start" => {
            let task_id = required_string(&args, "task_id")?;
            client
                .get_json(&format!("/v1/tasks/{task_id}/can_start"))
                .await
        }

        // ── WorkspaceGraph tools (P3) ─────────────────────────────────────
        "taskagent_workspacegraph_status" => client.get_json("/v1/workspacegraph/status").await,
        "taskagent_workspacegraph_context" => {
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
        "taskagent_workspacegraph_related" => {
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
        "taskagent_workspacegraph_search" => {
            let query = required_string(&args, "query")?;
            let limit = args.get("limit").and_then(|v| v.as_u64());
            let mut params: Vec<(&str, String)> = vec![("query", urlencode(&query))];
            match resolve_project_filter(&args, true, false, true)? {
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
        "taskagent_workspacegraph_impact" => {
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
        "taskagent_run_start" => {
            let plan_id = required_string(&args, "plan_id")?;
            let agent_id = required_string(&args, "agent_id")?;
            let mut body = json!({"plan_id": plan_id, "agent_id": agent_id});
            if let Some(parent) = args.get("parent_run_id").and_then(|v| v.as_str()) {
                body["parent_run_id"] = json!(parent);
            }
            client.post_json("/v1/runs", body).await
        }
        "taskagent_run_start_step" => {
            let run_id = required_string(&args, "run_id")?;
            let task_id = required_string(&args, "task_id")?;
            client
                .post_json(
                    &format!("/v1/runs/{run_id}/step/start"),
                    json!({"task_id": task_id}),
                )
                .await
        }
        "taskagent_run_finish_step" => {
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
        "taskagent_run_complete" => {
            let run_id = required_string(&args, "run_id")?;
            client
                .post_json(&format!("/v1/runs/{run_id}/complete"), json!({}))
                .await
        }
        "taskagent_run_abort" => {
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
        "taskagent_run_note_append" => {
            let run_id = required_string(&args, "run_id")?;
            let body = required_string(&args, "body")?;
            client
                .post_json(&format!("/v1/runs/{run_id}/notes"), json!({"body": body}))
                .await
        }
        "taskagent_run_log" => {
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
        "taskagent_run_notes_list" => {
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
        "taskagent_claim" => {
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
        "taskagent_release" => {
            let agent_id = required_string(&args, "agent_id")?;
            let task_id = required_string(&args, "task_id")?;
            client
                .delete_json(&format!("/v1/claims/{agent_id}/{task_id}"))
                .await
        }

        // ── Work-lease tools (parallel-agent file coordination) ──────────
        "taskagent_work_unit_create" => {
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
        "taskagent_work_unit_list" => {
            let task_id = required_string(&args, "task_id")?;
            client
                .get_json(&format!("/v1/tasks/{task_id}/work-units"))
                .await
        }
        "taskagent_work_unit_drain_next" => {
            let task_id = required_string(&args, "task_id")?;
            let mut body = json!({ "task_id": task_id });
            if let Some(ttl) = args.get("ttl_secs").and_then(|v| v.as_u64()) {
                body["ttl_secs"] = json!(ttl);
            }
            client.post_json("/v1/work-units/drain-next", body).await
        }
        "taskagent_work_unit_complete" => {
            let id = required_string(&args, "id")?;
            let mut body = json!({});
            if let Some(o) = args.get("outcome").and_then(|v| v.as_str()) {
                body["outcome"] = json!(o);
            }
            if let Some(a) = args.get("produced_artifacts").and_then(|v| v.as_array()) {
                body["produced_artifacts"] = json!(a);
            }
            client
                .post_json(&format!("/v1/work-units/{id}/complete"), body)
                .await
        }
        "taskagent_work_unit_release" => {
            let id = required_string(&args, "id")?;
            client
                .post_json(&format!("/v1/work-units/{id}/release"), json!({}))
                .await
        }
        "taskagent_project_settings_get" => {
            let project_id = required_string(&args, "project_id")?;
            client
                .get_json(&format!("/v1/projects/{project_id}/settings"))
                .await
        }
        "taskagent_project_settings_update" => {
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
        "taskagent_workspace_resolve" => {
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
            if let (Some(ws), Some(project_id)) = (
                workspace::global(),
                resp.get("project_id").and_then(|v| v.as_str()),
            ) {
                let _ = ws.set_default_project(project_id, Some(&root_path));
            }
            Ok(resp)
        }
        "taskagent_workspace_list" => client.get_json("/v1/workspace-registry").await,
        "taskagent_project_move_workspace" => {
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
        "taskagent_reserve_files" => {
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
        "taskagent_release_files" => {
            let agent_id = required_string(&args, "agent_id")?;
            let task_id = required_string(&args, "task_id")?;
            client
                .delete_json(&format!("/v1/leases/{agent_id}/{task_id}"))
                .await
        }
        "taskagent_active_work" => {
            let path = match args.get("project_id").and_then(|v| v.as_str()) {
                Some(p) if !p.is_empty() => format!("/v1/leases?project_id={p}"),
                _ => "/v1/leases".to_string(),
            };
            client.get_json(&path).await
        }

        // ── Project-wide ready pool ──────────────────────────────────────
        "taskagent_ready" => {
            let project_id = required_string(&args, "project_id")?;
            client
                .get_json(&format!("/v1/ready?project_id={project_id}"))
                .await
        }
        "taskagent_ready_drain" => {
            let project_id = required_string(&args, "project_id")?;
            let mut body = json!({});
            if let Some(ttl) = args.get("claim_ttl_secs").and_then(|v| v.as_u64()) {
                body["claim_ttl_secs"] = json!(ttl);
            }
            client
                .post_json(&format!("/v1/ready/drain?project_id={project_id}"), body)
                .await
        }
        "taskagent_doctor" => {
            let project_id = required_string(&args, "project_id")?;
            client
                .get_json(&format!("/v1/doctor?project_id={project_id}"))
                .await
        }
        "taskagent_suggest_files" => {
            let task_id = required_string(&args, "task_id")?;
            client
                .get_json(&format!("/v1/leases/suggest?task_id={task_id}"))
                .await
        }

        // ── Session tools (W3.2 / Linear B.1) ────────────────────────────
        "taskagent_session_start" => {
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
        "taskagent_session_get" => {
            let id = required_string(&args, "id")?;
            client.get_json(&format!("/v1/sessions/{id}")).await
        }
        "taskagent_session_list" => {
            let agent_id = args
                .get("agent_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| client.agent_id());
            client
                .get_json(&format!("/v1/sessions?agent_id={agent_id}"))
                .await
        }
        "taskagent_session_end" => {
            let id = required_string(&args, "id")?;
            client
                .post_json(&format!("/v1/sessions/{id}/end"), json!({}))
                .await
        }
        "taskagent_session_set_plan" => {
            let id = required_string(&args, "id")?;
            let steps = args
                .get("steps")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("`steps` (array, max 100) is required"))?;
            client
                .post_json(&format!("/v1/sessions/{id}/plan"), json!({"steps": steps}))
                .await
        }
        "taskagent_session_artifact" => {
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
        "taskagent_session_artifacts_list" => {
            let id = required_string(&args, "id")?;
            client
                .get_json(&format!("/v1/sessions/{id}/artifacts"))
                .await
        }

        // ── Signal tools (W3.2 / Linear B.5) ─────────────────────────────
        "taskagent_signal_send" => {
            let run_id = required_string(&args, "run_id")?;
            let kind = args
                .get("kind")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("`kind` (signal object) is required"))?;
            client
                .post_json(&format!("/v1/runs/{run_id}/signals"), json!({"kind": kind}))
                .await
        }
        "taskagent_signal_respond" => {
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
        "taskagent_link" => {
            let from = required_string(&args, "from")?;
            let to = required_string(&args, "to")?;
            let kind = required_string(&args, "kind")?;
            let mut body = serde_json::json!({"from": from, "to": to, "kind": kind});
            if let Some(ccid) = args.get("client_command_id").and_then(|v| v.as_str()) {
                body["client_command_id"] = serde_json::json!(ccid);
            }
            client.post_json("/v1/relations", body).await
        }
        "taskagent_unlink" => {
            let relation_id = required_string(&args, "relation_id")?;
            client
                .delete_json(&format!("/v1/relations/{relation_id}"))
                .await
        }
        "taskagent_relations" => {
            let task_id = required_string(&args, "task_id")?;
            client
                .get_json(&format!("/v1/tasks/{task_id}/relations"))
                .await
        }

        // ── Document tools (PR1 §7) ───────────────────────────────────────
        "taskagent_doc_create" => {
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
            client
                .post_json("/v1/documents", json!({ "new_doc": new_doc }))
                .await
        }
        "taskagent_doc_get" => {
            let id = required_string(&args, "document_id")?;
            client.get_json(&format!("/v1/documents/{id}")).await
        }
        "taskagent_doc_append" => {
            let id = required_string(&args, "document_id")?;
            let content = required_string(&args, "content")?;
            client
                .post_json(
                    &format!("/v1/documents/{id}/append"),
                    json!({ "content": content }),
                )
                .await
        }
        "taskagent_doc_replace" => {
            let id = required_string(&args, "document_id")?;
            let content = required_string(&args, "content")?;
            client
                .patch_json(
                    &format!("/v1/documents/{id}"),
                    json!({ "content": content }),
                )
                .await
        }
        "taskagent_doc_rename" => {
            let id = required_string(&args, "document_id")?;
            let title = required_string(&args, "title")?;
            client
                .patch_json(&format!("/v1/documents/{id}"), json!({ "title": title }))
                .await
        }
        "taskagent_doc_archive" => {
            let id = required_string(&args, "document_id")?;
            client
                .post_json(&format!("/v1/documents/{id}/archive"), json!({}))
                .await
        }
        "taskagent_doc_list" => {
            // `project_id` falls back to the workspace default. The URL
            // path requires a project id, so we bail with a friendly error
            // if neither is set instead of producing a malformed URL.
            let project_id = match resolve_project_filter(&args, false, false, true)? {
                ProjectFilter::Project(pid) => pid,
                ProjectFilter::None => {
                    anyhow::bail!(
                        "`project_id`, `project_scope`, or `scope_path` is required and no taskagent scope is resolved"
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
        "taskagent_history_list" => {
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
        "taskagent_history_get" => {
            let id = required_string(&args, "version_id")?;
            client.get_json(&format!("/v1/history/{id}")).await
        }
        "taskagent_history_compare" => {
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
        "taskagent_history_latest" => {
            let limit = optional_u32(&args, "limit").unwrap_or(50);
            client
                .get_json(&format!("/v1/history/latest?limit={limit}"))
                .await
        }
        "taskagent_history_summary" => {
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
        "taskagent_history_rollback" => {
            let id = required_string(&args, "version_id")?;
            client
                .post_json(&format!("/v1/history/{id}/rollback"), json!({}))
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
                    "project_id": {"type":"string"}
                },
                "required":["title"]
            },
            "scope": {
                "type":"string",
                "description":"Named taskagent scope (usually repo folder name) used when task.project_id is omitted."
            },
            "project_scope": {
                "type":"string",
                "description":"Named taskagent scope (alias-safe form; preferred when a tool already has a `scope` option)."
            },
            "scope_path": {
                "type":"string",
                "description":"Filesystem path used to resolve the nearest configured taskagent scope."
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
            "scope": {"type":"string", "description":"Named taskagent scope (usually repo folder name)."},
            "project_scope": {"type":"string", "description":"Named taskagent scope (alias-safe form)."},
            "scope_path": {"type":"string", "description":"Filesystem path used to resolve the nearest configured taskagent scope."}
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
            "scope": {"type":"string", "description":"Named taskagent scope (usually repo folder name)."},
            "project_scope": {"type":"string", "description":"Named taskagent scope (alias-safe form)."},
            "scope_path": {"type":"string", "description":"Filesystem path used to resolve the nearest configured taskagent scope."}
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
            "scope": {"type":"string", "description":"Destination taskagent scope (usually repo folder name)."},
            "project_scope": {"type":"string", "description":"Destination taskagent scope (alias-safe form)."},
            "scope_path": {"type":"string", "description":"Filesystem path used to resolve the destination taskagent scope."}
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

fn schema_ai_parse() -> Value {
    json!({
        "type":"object",
        "properties": {
            "input": {"type":"string"},
            "use_research_provider": use_research_provider_property(),
        },
        "required":["input"]
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

fn schema_ai_decompose() -> Value {
    json!({
        "type":"object",
        "properties": {
            "task_id": {"type":"string","description":"Task identifier"},
            "hint": {
                "type":"string",
                "description":"Optional free-form guidance appended to the prompt (e.g. `expansion_hint` from `taskagent_ai_analyze_complexity`)."
            },
            "use_research_provider": use_research_provider_property(),
        },
        "required":["task_id"]
    })
}

fn schema_ai_research() -> Value {
    json!({
        "type":"object",
        "properties": {
            "query": {"type":"string", "description":"Free-form research question."},
            "context_task_ids": {
                "type":"array",
                "items": {"type":"string"},
                "description":"Optional list of task ids whose title+description are added to the prompt as grounding context."
            },
            "save_to_task_id": {
                "type":"string",
                "description":"When set, the answer is also persisted as a Research comment on this task (uses §3.8.8 CommentKind)."
            },
            "use_research_provider": use_research_provider_property(),
        },
        "required":["query"]
    })
}

fn schema_ai_scope() -> Value {
    json!({
        "type":"object",
        "properties": {
            "task_id": {"type":"string", "description":"Task identifier"},
            "direction": {
                "type":"string",
                "enum": ["up", "down", "broaden", "narrow"],
                "description":"`up`/`broaden` widens the task into an epic-style framing; `down`/`narrow` collapses it into a single concrete action."
            },
            "strength": {
                "type":"string",
                "enum": ["light", "regular", "heavy"],
                "description":"Reserved for §3.8.7a — currently accepted but ignored by the server."
            },
            "use_research_provider": use_research_provider_property(),
        },
        "required":["task_id", "direction"]
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
                "description":"Named taskagent scope (usually repo folder name)."
            },
            "project_scope": {
                "type":"string",
                "description":"Named taskagent scope (alias-safe form)."
            },
            "scope_path": {
                "type":"string",
                "description":"Filesystem path used to resolve the nearest configured taskagent scope."
            },
            "status": {
                "type":"string",
                "description": "Required. Single status (`inbox`/`todo`/`in_progress`/`in_review`/`done`/`cancelled`), comma-separated list (e.g. `todo,in_progress`), shortcut `active` (non-terminal), or `all`. **Ask the user before `all`** — full archive can be a very heavy response."
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
                "description":"Named taskagent scope (use this instead of `scope`, which filters search domains)."
            },
            "scope_path": {
                "type":"string",
                "description":"Filesystem path used to resolve the nearest configured taskagent scope."
            },
            "limit": {
                "type":"integer",
                "minimum":1,
                "maximum":100,
                "default":20
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
                "description":"Named taskagent scope."
            },
            "scope_path": {
                "type":"string",
                "description":"Filesystem path used to resolve the nearest configured taskagent scope."
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
                "description": "Workspace or repository path to bind. Relative paths are resolved from TASKAGENT_WORKSPACE / process CWD. Omit only when MCP is running inside the repository scope."
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
                "description":"Named taskagent scope (usually repo folder name)."
            },
            "project_scope": {
                "type":"string",
                "description":"Named taskagent scope (alias-safe form)."
            },
            "scope_path": {
                "type":"string",
                "description":"Filesystem path used to resolve the nearest configured taskagent scope."
            },
            "status": {
                "type":"string",
                "description": "Required. `draft`/`active`/`completed`/`abandoned`, comma-separated list, or `all`. **Ask the user before `all`** — full archive can be a very heavy response."
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
                "description":"Named taskagent scope (usually repo folder name)."
            },
            "project_scope": {
                "type":"string",
                "description":"Named taskagent scope (alias-safe form)."
            },
            "scope_path": {
                "type":"string",
                "description":"Filesystem path used to resolve the nearest configured taskagent scope."
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
            "produced_artifacts": {"type":"array","items":{"type":"string"}}
        },
        "required":["id"]
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
                "description":"Defaults to this MCP process agent id (see taskagent_workspace_info.mcp_agent_id)."
            },
            "parent_agent_id": {"type":"string"},
            "metadata":        {
                "type":"object",
                "description":"Traceability payload. Recommended keys: client, model, chat_id, transcript_path, workspace_path. Env defaults: TASKAGENT_CLIENT, TASKAGENT_MODEL, TASKAGENT_CHAT_ID, TASKAGENT_TRANSCRIPT_PATH.",
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
            "content":    {"type":"string","description":"Initial markdown body. Defaults to empty when omitted."}
        },
        "required":["project_id","kind","title"]
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
            "scope": {"type":"string", "description":"Named taskagent scope (usually repo folder name)."},
            "project_scope": {"type":"string", "description":"Named taskagent scope (alias-safe form)."},
            "scope_path": {"type":"string", "description":"Filesystem path used to resolve the nearest configured taskagent scope."},
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

fn resolve_project_filter(
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

    let ws = match workspace::global() {
        Some(ws) => ws,
        None => return Ok(ProjectFilter::None),
    };

    if let Some(project_scope) = args.get("project_scope").and_then(|v| v.as_str()) {
        return resolve_named_scope(ws, project_scope);
    }
    if allow_scope_alias {
        if let Some(scope) = args.get("scope").and_then(|v| v.as_str()) {
            return resolve_named_scope(ws, scope);
        }
    }
    if let Some(scope_path) = args.get("scope_path").and_then(|v| v.as_str()) {
        return ws
            .project_for_path(scope_path)
            .map(ProjectFilter::Project)
            .ok_or_else(|| {
                anyhow::anyhow!("no taskagent scope configured for path `{scope_path}`")
            });
    }

    ws.inferred_project().map(|p| match p {
        Some(project_id) => ProjectFilter::Project(project_id),
        None => ProjectFilter::None,
    })
}

fn resolve_named_scope(ws: &workspace::Workspace, scope: &str) -> anyhow::Result<ProjectFilter> {
    ws.project_for_scope(scope)?
        .map(ProjectFilter::Project)
        .ok_or_else(|| anyhow::anyhow!("unknown taskagent scope `{scope}`"))
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
        "reason": "No default TaskAgent project is resolved for this MCP workspace. To avoid a token-heavy all-project task listing, choose a project first.",
        "requested_status": requested_status,
        "projects": projects,
        "next_step": "Ask the user which project to use, then call taskagent_project_use with that project_id. After that, retry taskagent_list with the same status; the saved default project will be reused by later calls.",
        "next_tool": {
            "name": "taskagent_project_use",
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
    if let Some(t) = task.as_object_mut() {
        match resolve_project_filter(args, false, true, true)? {
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
/// Mirrors `taskagent_domain::CommentKind::FromStr`: accepts the
/// snake_case canonical form (`"research"`), the PascalCase Rust
/// variant name (`"Research"`), and tolerates surrounding whitespace
/// and case. The mcp crate doesn't depend on `taskagent-domain`, so
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
    fn catalogue_includes_required_tools() {
        let names: Vec<&str> = tool_definitions().iter().map(|t| t.name).collect();
        assert!(
            names.len() >= 10,
            "AC-7: must expose ≥10 tools (got {})",
            names.len()
        );
        for required in [
            "taskagent_subscribe_project",
            "taskagent_inbox_pull",
            "taskagent_comment",
            "taskagent_reopen",
            "taskagent_project_list",
            "taskagent_project_create",
            "taskagent_project_use",
            "taskagent_move_project",
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
            "taskagent_plan_create",
            "taskagent_plan_update",
            "taskagent_plan_get",
            "taskagent_plan_list",
            "taskagent_plan_add_task",
            "taskagent_plan_remove_task",
            "taskagent_plan_reorder",
            "taskagent_plan_archive",
            "taskagent_plan_next_task",
            // Run tools
            "taskagent_run_start",
            "taskagent_run_start_step",
            "taskagent_run_finish_step",
            "taskagent_run_complete",
            "taskagent_run_abort",
            // Claim tools
            "taskagent_claim",
            "taskagent_release",
            // Work-lease tools
            "taskagent_reserve_files",
            "taskagent_release_files",
            "taskagent_active_work",
            // Project-wide ready pool
            "taskagent_ready",
            "taskagent_ready_drain",
            // Doctor + file suggestion
            "taskagent_doctor",
            "taskagent_suggest_files",
            // Session tools (Linear B.1)
            "taskagent_session_start",
            "taskagent_session_get",
            "taskagent_session_list",
            "taskagent_session_end",
            "taskagent_session_set_plan",
            "taskagent_session_artifact",
            "taskagent_session_artifacts_list",
            // Signal tools (Linear B.5)
            "taskagent_signal_send",
            "taskagent_signal_respond",
            // Relation tools (§3.2 W3.2 / AC-9)
            "taskagent_link",
            "taskagent_unlink",
            "taskagent_relations",
        ] {
            assert!(names.contains(&required), "missing W3.2 tool: {required}");
        }
    }

    #[test]
    fn catalogue_includes_document_tools() {
        let names: Vec<&str> = tool_definitions().iter().map(|t| t.name).collect();
        for required in [
            "taskagent_doc_create",
            "taskagent_doc_get",
            "taskagent_doc_append",
            "taskagent_doc_replace",
            "taskagent_doc_rename",
            "taskagent_doc_archive",
            "taskagent_doc_list",
        ] {
            assert!(names.contains(&required), "missing PR1 tool: {required}");
        }
    }

    #[test]
    fn catalogue_includes_ai_analyze_complexity() {
        let names: Vec<&str> = tool_definitions().iter().map(|t| t.name).collect();
        assert!(
            names.contains(&"taskagent_ai_analyze_complexity"),
            "§3.8.3: missing tool taskagent_ai_analyze_complexity in {names:?}"
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
            "default profile grew to {} tools — keep it compact",
            default.len()
        );
        for required in [
            "taskagent_capture",
            "taskagent_create",
            "taskagent_list",
            "taskagent_get",
            "taskagent_search",
            "taskagent_comment",
            "taskagent_set_status",
            "taskagent_complete",
            "taskagent_plan_create",
            "taskagent_plan_get",
            "taskagent_plan_drain_next",
            "taskagent_claim",
            "taskagent_release",
            "taskagent_run_start",
            "taskagent_run_complete",
            "taskagent_link",
        ] {
            assert!(default.contains(&required), "default must keep {required}");
        }
        // Advanced/destructive surfaces stay out of default.
        for excluded in [
            "taskagent_delete",
            "taskagent_project_delete",
            "taskagent_history_rollback",
            "taskagent_workspacegraph_search",
            "taskagent_ai_decompose",
            "taskagent_session_start",
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
            "taskagent_delete",
            "taskagent_project_delete",
            "taskagent_plan_archive",
            "taskagent_doc_archive",
            "taskagent_unlink",
            "taskagent_history_rollback",
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
        for expected in [
            "taskagent_ai_parse",
            "taskagent_ai_decompose",
            "taskagent_ai_analyze_complexity",
            "taskagent_ai_scope",
            "taskagent_research",
        ] {
            assert!(
                open_world.contains(&expected),
                "{expected} must be open-world"
            );
        }
    }

    #[test]
    fn serialized_tool_matches_mcp_shape() {
        let tools = tool_definitions();
        let sample = tools.iter().find(|t| t.name == "taskagent_list").unwrap();
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
            "taskagent_delete",
            ToolProfile::Default
        ));
        assert!(!tool_hidden_in_profile(
            "taskagent_list",
            ToolProfile::Default
        ));
        assert!(!tool_hidden_in_profile(
            "taskagent_delete",
            ToolProfile::Full
        ));
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

        std::env::remove_var("TASKAGENT_MCP_PROFILE");
        assert_eq!(ToolProfile::from_env(), ToolProfile::Default);
        std::env::set_var("TASKAGENT_MCP_PROFILE", "full");
        assert_eq!(ToolProfile::from_env(), ToolProfile::Full);
        std::env::set_var("TASKAGENT_MCP_PROFILE", "garbage");
        assert_eq!(ToolProfile::from_env(), ToolProfile::Default);
        std::env::remove_var("TASKAGENT_MCP_PROFILE");
    }
}
