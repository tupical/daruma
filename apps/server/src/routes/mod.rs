//! Axum router wiring all HTTP + WS endpoints.
//!
//! ## URL layout
//!
//! | Prefix   | Paths                                                            | Auth          |
//! |----------|------------------------------------------------------------------|---------------|
//! | (root)   | `/healthz`                                                       | none          |
//! | `/v1`    | `/healthz`, `/ws`                                                | none / subproto |
//! | `/v1`    | `/tokens`, `/downloads/daruma/{platform}`, `/ai/analyze-complexity`, …   | bearer        |
//! | (legacy) | same paths without `/v1` prefix                                  | bearer        |
//! |          | (also carry `Deprecation: true` + `Sunset` headers)              |               |
//!
//! The bearer middleware lives in `middleware/auth.rs`. WS auth uses the
//! `Sec-WebSocket-Protocol` subprotocol — implemented in W2.3.

pub mod downloads;
pub mod pairing;
pub mod relations;
pub mod shell;
pub mod workspacegraph;

use std::sync::Arc;

use axum::{
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::IntoResponse,
    routing::{delete, get, patch, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use daruma_auth::{
    generate, AuthContext, Capabilities, Capability, NewTokenSpec, ProjectFilter, TokenKind,
    TokenScope, TokenStore,
};
use daruma_core::{
    plan_concurrency::NextTaskResolver,
    plan_readiness,
    repos::{AgentClaimRepository, ExternalRefRepository, PlanRepository},
    search::{FtsSearchProvider, SearchProvider, SearchQuery as CoreSearchQuery, SearchScope},
    Command, CommandEnvelope,
};
use daruma_domain::{
    slugify_title, Actor, AgentSessionPlanStep, CommentKind, CommentPatch, ComplexityHint,
    Document, DocumentKind, NewComment, NewDocument, NewPlan, Plan, PlanPatch, PlanStatus,
    RunOutcome, SessionArtifactKind, SignalKind, Status, Task, TaskPatch, TriageState, Verb,
};
use daruma_events::{Event, EventEnvelope};
use daruma_mcp::{
    dispatch_request_with_profile as dispatch_mcp_request, ApiClient, JsonRpcRequest,
};
use daruma_shared::{
    AgentId, AgentSessionId, CommentId, CoreError, DocumentId, EvidenceId, PlanId, ProjectId,
    RuleId, RunId, TaskId, TokenId, WebhookId,
};
use daruma_storage::{ClaimOutcome, ReserveOutcome};
use daruma_webhooks::{NewWebhook, WebhookPatch, WebhookStore};

use daruma_api_dto::{MutationResponse, MutationWarning};

use crate::{
    error::ApiError,
    middleware::{
        auth::require_auth, auth::AuthLayer, rate_limit::enforce_pairing_rate_limit,
        rate_limit::enforce_rate_limit, request_id::request_id_middleware,
    },
    state::AppState,
    ws::ws_handler,
};

// ── Sunset header computation ──────────────────────────────────────────────────

/// Compute today (UTC) + 30 days as an RFC 7231 IMF-fixdate [`HeaderValue`].
fn compute_sunset() -> HeaderValue {
    use std::time::{SystemTime, UNIX_EPOCH};

    let epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let target = epoch_secs + 30 * 24 * 3600;

    let total_days = target / 86400;
    let rem = target % 86400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let dow = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][((total_days + 4) % 7) as usize];
    let (year, mon, day) = epoch_days_to_ymd(total_days);
    let mon_name = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][mon - 1];

    let s = format!("{dow}, {day:02} {mon_name} {year} {hh:02}:{mm:02}:{ss:02} GMT");
    HeaderValue::from_str(&s).expect("sunset value is valid ASCII")
}

fn epoch_days_to_ymd(mut d: u64) -> (u64, usize, u64) {
    let mut y = 1970u64;
    loop {
        let n = if is_leap_year(y) { 366 } else { 365 };
        if d < n {
            break;
        }
        d -= n;
        y += 1;
    }
    let mlen: [u64; 12] = [
        31,
        if is_leap_year(y) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0usize;
    while m < 11 && d >= mlen[m] {
        d -= mlen[m];
        m += 1;
    }
    (y, m + 1, d + 1)
}

fn is_leap_year(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

// ── Router ─────────────────────────────────────────────────────────────────────

/// Build the complete Axum [`Router`].
pub fn router(state: AppState) -> Router {
    let sunset: Arc<HeaderValue> = Arc::new(compute_sunset());
    let auth_layer = AuthLayer::new(state.auth_store.clone());

    let sv = Arc::clone(&sunset);
    let deprecation_layer = axum::middleware::from_fn(move |req: Request, next: Next| {
        let sv = Arc::clone(&sv);
        async move {
            let mut res = next.run(req).await;
            let h = res.headers_mut();
            h.insert("deprecation", HeaderValue::from_static("true"));
            h.insert("sunset", (*sv).clone());
            res
        }
    });

    // /v1 mount — /healthz + /ws stay public, the rest is gated by auth.
    let v1 = Router::new()
        .route("/healthz", get(healthz))
        .merge(public_routes(state.clone()))
        .merge(authed_routes(state.clone(), auth_layer.clone()));

    // Legacy aliases (root-mounted) — /ws + every authed endpoint.
    // `/healthz` is **not** aliased; it lives at root only.
    let legacy = public_routes(state.clone())
        .merge(authed_routes(state.clone(), auth_layer))
        .layer(deprecation_layer);

    // The browser UI was extracted to the standalone `daruma-web` repo
    // (Leptos CSR → WASM). This server is a bare API + MCP backend and no
    // longer bundles/serves static web assets — deploy the UI separately and
    // point it at `/v1/*` + `/v1/ws`.
    let shell = Router::new()
        .route(
            "/.well-known/daruma-shell.json",
            get(shell::host_shell_config),
        )
        .route("/workspaces", get(shell::workspace_switcher))
        .with_state(state.clone());

    Router::new()
        .route("/healthz", get(healthz))
        .merge(shell)
        .nest("/v1", v1)
        .merge(legacy)
        .layer(axum::middleware::from_fn(request_id_middleware))
}

/// Endpoints that do not require bearer authentication. Currently only
/// `/ws` — the WS connection is authenticated through
/// `Sec-WebSocket-Protocol` (W2.3).
fn public_routes(state: AppState) -> Router {
    // Pairing route gets its own IP-keyed rate limiter (5 req/min) because it
    // is unauthenticated — the single-use token is the credential — and must
    // not be brute-forced.  The WS route has no such concern.
    let pairing_route = Router::new()
        .route("/devices/pair", post(pairing::pair_device))
        .layer(axum::middleware::from_fn_with_state(
            state.rate_limiter.clone(),
            enforce_pairing_rate_limit,
        ))
        .with_state(state.clone());

    Router::new()
        .route("/ws", get(ws_handler))
        .merge(pairing_route)
        .with_state(state)
}

/// Endpoints behind the bearer-token middleware. All require an
/// [`AuthContext`]; each handler additionally calls `ctx.require(...)` for
/// the capability it needs.
fn authed_routes(state: AppState, auth_layer: AuthLayer) -> Router {
    Router::new()
        .route("/tasks", get(list_tasks))
        .route("/tasks/{id}", get(get_task))
        .route("/search", get(search))
        .route("/projects", get(list_projects))
        .route("/projects/{id}", axum::routing::delete(delete_project))
        .route("/projects/{id}/workspace", patch(move_project_to_workspace))
        .route(
            "/projects/{id}/settings",
            get(get_project_settings).patch(patch_project_settings),
        )
        .route(
            "/projects/{id}/triage",
            get(list_project_triage).patch(patch_project_triage),
        )
        .route("/rules", get(list_rules).post(create_rule))
        .route(
            "/rules/{id}",
            get(get_rule).patch(patch_rule).delete(disable_rule),
        )
        .route("/evidence", get(list_evidence).post(record_evidence))
        .route("/evidence/{id}", get(get_evidence))
        // ── Audit primitives ────────────────────────────────────────────────
        .route(
            "/audit/findings",
            get(list_audit_findings).post(record_audit_finding),
        )
        .route("/audit/findings/{id}", get(get_audit_finding))
        .route(
            "/audit/findings/{id}/status",
            post(set_audit_finding_status),
        )
        .route(
            "/audit/findings/resolve-missing",
            post(resolve_missing_findings),
        )
        .route("/audit/heuristics/stuck-tasks", get(audit_stuck_tasks))
        .route(
            "/audit/heuristics/duplicate-tasks",
            get(audit_duplicate_tasks),
        )
        .route(
            "/audit/heuristics/unread-documents",
            get(audit_unread_documents),
        )
        .route(
            "/workspace-registry",
            get(list_logical_workspaces).post(create_logical_workspace),
        )
        .route(
            "/workspace-registry/resolve",
            post(resolve_workspace_context),
        )
        .route("/mcp", post(mcp_http))
        .route("/commands", post(dispatch_command))
        .route("/events", get(list_events))
        .route("/events/replica", post(append_replica_events))
        .route("/history", get(list_entity_history))
        .route("/history/latest", get(latest_history))
        .route("/history/summary", get(history_summary))
        .route("/history/compare", get(compare_history_versions))
        .route("/history/{id}", get(get_history_version))
        .route("/history/{id}/rollback", post(rollback_history_version))
        .route(
            "/ai/analyze-complexity/{plan_id}",
            post(ai_analyze_complexity),
        )
        .route("/complexity-hints", post(upsert_complexity_hints))
        .route("/tasks/{task_id}/activity", get(list_task_activity))
        .route(
            "/tasks/{task_id}/comments",
            post(add_comment).get(list_task_comments),
        )
        .route("/tasks/{id}/can_start", get(get_can_start))
        .route("/comments/{id}", patch(edit_comment).delete(delete_comment))
        .route("/tasks/{id}/triage", patch(patch_task_triage))
        .route("/tokens", post(create_token).get(list_tokens))
        .route("/tokens/{id}", delete(revoke_token))
        // Pairing: issue a QR ticket (requires TokenWrite capability).
        .route("/devices/pair/ticket", get(pairing::issue_pairing_ticket))
        .route(
            "/downloads/daruma/{platform}",
            get(downloads::download_daruma_mcp),
        )
        .route("/downloads/daruma", get(downloads::mcp_download_info))
        .route("/agents/{agent_id}/inbox", get(agent_inbox))
        .route("/agents/{agent_id}/inbox/ack", post(agent_inbox_ack))
        .route("/webhooks", post(create_webhook).get(list_webhooks))
        .route(
            "/webhooks/{id}",
            patch(patch_webhook).delete(delete_webhook),
        )
        // ── Plan routes (W3.1) ──────────────────────────────────────────────
        .route("/plans", post(create_plan).get(list_plans))
        .route("/plans/{id}", patch(update_plan).get(get_plan))
        .route("/plans/{id}/tasks", post(add_plan_task))
        .route("/plans/{id}/tasks/{task_id}", delete(remove_plan_task))
        .route("/plans/{id}/reorder", post(reorder_plan))
        .route("/plans/{id}/archive", post(archive_plan))
        .route("/plans/{id}/status", post(set_plan_status))
        .route("/plans/{id}/next-task", get(get_next_task))
        .route("/plans/{id}/drain-next", post(drain_next_task))
        .route("/plans/{id}/progress", get(get_plan_progress))
        .route("/plans/{id}/graph", get(get_plan_graph))
        .route("/plans/{id}/fanout", get(get_plan_fanout))
        // ── Run routes (W3.1) ───────────────────────────────────────────────
        .route("/runs", post(start_run))
        .route("/runs/{id}/step/start", post(run_start_step))
        .route("/runs/{id}/step/finish", post(run_finish_step))
        .route("/runs/{id}/complete", post(complete_run))
        .route("/runs/{id}/abort", post(abort_run))
        .route("/runs/{id}/signal", post(send_run_signal))
        .route("/runs/{id}/signal/respond", post(respond_run_signal))
        .route(
            "/runs/{id}/notes",
            post(append_run_note).get(list_run_notes),
        )
        // ── Session routes (W3.1) ───────────────────────────────────────────
        .route("/sessions", post(start_session).get(list_sessions))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}/end", post(end_session))
        .route(
            "/sessions/{id}/plan-steps",
            patch(update_session_plan_steps),
        )
        .route(
            "/sessions/{id}/artifacts",
            post(attach_session_artifact).get(list_session_artifacts),
        )
        // ── Claim routes (W3.1) ─────────────────────────────────────────────
        .route("/claims", post(acquire_claim))
        .route("/claims/{agent_id}/{task_id}", delete(release_claim))
        // ── Work-lease routes (parallel-agent file coordination) ────────────
        .route("/leases", post(reserve_files).get(active_work))
        .route("/work-units", post(create_work_unit))
        .route("/work-units/drain-next", post(work_unit_drain_next))
        .route("/work-units/{id}/complete", post(complete_work_unit))
        .route("/work-units/{id}/release", post(release_work_unit))
        .route("/work-units/{id}/handoffs", get(list_work_unit_handoffs))
        .route("/tasks/{id}/work-units", get(list_task_work_units))
        .route(
            "/agents/{agent_id}/capabilities",
            get(list_agent_capabilities).put(put_agent_capability),
        )
        .route(
            "/agents/{agent_id}/capabilities/{capability}",
            delete(delete_agent_capability),
        )
        .route("/handoffs", post(request_handoff))
        .route("/handoffs/{id}/accept", post(accept_handoff))
        .route("/handoffs/{id}/reject", post(reject_handoff))
        .route("/leases/{agent_id}/{task_id}", delete(release_files))
        .route("/leases/suggest", get(suggest_files))
        // ── Project-wide ready pool (drain across all active plans) ──────────
        .route("/ready", get(project_ready))
        .route("/ready/drain", post(project_ready_drain))
        // ── Doctor: reconcile stale parallel-agent state ────────────────────
        .route("/doctor", get(project_doctor))
        // ── Relation routes (§3.2 W3.1) ─────────────────────────────────────
        .route(
            "/relations",
            post(relations::link).get(relations::list_for_tasks),
        )
        .route("/relations/query", post(relations::query_for_tasks))
        .route("/relations/{id}", axum::routing::delete(relations::unlink))
        .route("/tasks/{id}/relations", get(relations::list_for_task))
        .route("/tasks/{id}/plans", get(list_task_plans))
        // ── Document routes (PR1 §8) ────────────────────────────────────────
        .route("/documents", post(create_document))
        .route("/documents/{id}", patch(patch_document).get(get_document))
        .route("/documents/{id}/append", post(append_document))
        .route("/documents/{id}/archive", post(archive_document))
        .route(
            "/projects/{project_id}/documents",
            get(list_project_documents),
        )
        // ── WorkspaceGraph routes (P3) ────────────────────────────────────────
        .route("/workspacegraph/status", get(workspacegraph::status))
        .route("/workspacegraph/context", get(workspacegraph::context))
        .route("/workspacegraph/related", get(workspacegraph::related))
        .route("/workspacegraph/search", get(workspacegraph::search))
        .route("/workspacegraph/impact", get(workspacegraph::impact))
        .layer(axum::middleware::from_fn_with_state(
            state.rate_limiter.clone(),
            enforce_rate_limit,
        ))
        .layer(axum::middleware::from_fn_with_state(
            auth_layer,
            require_auth,
        ))
        .with_state(state)
}

// ── Public handlers ───────────────────────────────────────────────────────────

/// Public REST API version advertised to clients. Bumped only on a /v2 cut.
pub const API_VERSION: &str = "v1";
const DEFAULT_COLLECTION_LIMIT: usize = 10;
const MAX_COLLECTION_LIMIT: usize = 100;

async fn healthz() -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "core_version": daruma_core::VERSION,
        "api_version": API_VERSION,
    }))
}

// ── Task / project / event handlers ───────────────────────────────────────────

#[derive(Deserialize, Default)]
struct ListTasksQuery {
    /// `<uuid>` filters to that project; `inbox` filters to tasks with no
    /// project; absent returns every task.
    project_id: Option<String>,
    /// **Required.** Comma-separated statuses (`todo,in_progress`), the
    /// shortcut `active` (all non-terminal), or `all` (every status).
    status: Option<String>,
    /// Max rows to return. Defaults to 10, capped at 100.
    limit: Option<usize>,
}

async fn list_tasks(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<ListTasksQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;

    let status_filter = parse_status_filter(q.status.as_deref())?;
    let filter = status_filter.as_deref().unwrap_or(&[]);

    let limit = bounded_collection_limit(q.limit);
    let mut tasks = match q.project_id.as_deref() {
        None => state.tasks.list_all_filtered(filter).await,
        Some("inbox") => state.tasks.list_by_project_filtered(None, filter).await,
        Some(raw) => {
            let pid = raw.parse::<ProjectId>().map_err(|_| {
                ApiError::from(CoreError::validation(format!("invalid project id: {raw}")))
            })?;
            state
                .tasks
                .list_by_project_filtered(Some(pid), filter)
                .await
        }
    }
    .map_err(ApiError::from)?;
    tasks.truncate(limit);
    Ok(Json(tasks))
}

#[derive(Deserialize, Default)]
struct SearchHttpQuery {
    query: String,
    /// Comma-separated subset of `tasks`, `comments`, `plans`.
    scope: Option<String>,
    project_id: Option<String>,
    limit: Option<usize>,
}

async fn search(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<SearchHttpQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let query = q.query.trim();
    if query.is_empty() {
        return Err(ApiError::from(CoreError::validation(
            "query must not be empty",
        )));
    }

    let scopes = parse_search_scopes(q.scope.as_deref())?;
    if scopes.contains(&SearchScope::Tasks) || scopes.contains(&SearchScope::Comments) {
        auth.require(Capability::TaskRead)
            .map_err(ApiError::from_missing_cap)?;
    }
    if scopes.contains(&SearchScope::Plans) {
        auth.require(Capability::PlanRead)
            .map_err(ApiError::from_missing_cap)?;
    }

    let limit = bounded_collection_limit(q.limit);
    let project_id = parse_search_project(q.project_id.as_deref())?;
    let provider = FtsSearchProvider::new(
        state.tasks.clone(),
        state.comments.clone(),
        state.plans.clone(),
    );
    let hits = provider
        .search(CoreSearchQuery {
            query: query.to_string(),
            scopes,
            project_id,
            limit,
        })
        .await
        .map_err(ApiError::from)?;

    Ok(Json(hits))
}

fn bounded_collection_limit(limit: Option<usize>) -> usize {
    limit
        .unwrap_or(DEFAULT_COLLECTION_LIMIT)
        .clamp(1, MAX_COLLECTION_LIMIT)
}

fn parse_search_scopes(raw: Option<&str>) -> Result<Vec<SearchScope>, ApiError> {
    let raw = raw.unwrap_or("tasks,comments,plans");
    let mut scopes = Vec::new();
    for token in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let scope = match token {
            "tasks" => SearchScope::Tasks,
            "comments" => SearchScope::Comments,
            "plans" => SearchScope::Plans,
            other => {
                return Err(ApiError::from(CoreError::validation(format!(
                    "unknown search scope: {other}"
                ))))
            }
        };
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    if scopes.is_empty() {
        return Err(ApiError::from(CoreError::validation(
            "scope must include at least one value",
        )));
    }
    Ok(scopes)
}

fn parse_search_project(raw: Option<&str>) -> Result<Option<ProjectId>, ApiError> {
    match raw {
        None | Some("") | Some("all") => Ok(None),
        Some(pid) => pid.parse::<ProjectId>().map(Some).map_err(|_| {
            ApiError::from(CoreError::validation(format!("invalid project id: {pid}")))
        }),
    }
}

/// Parse the required `status` query parameter into a list of `Status` values.
///
/// Returns `Ok(None)` for `all` (no SQL status predicate), `Ok(Some(vec))`
/// for explicit filters, and `Err` (400) when absent, empty, or unknown.
/// The shortcut `active` expands to all non-terminal statuses.
fn parse_status_filter(
    raw: Option<&str>,
) -> Result<Option<Vec<daruma_domain::Status>>, ApiError> {
    use daruma_domain::Status;
    let Some(raw) = raw else {
        return Err(ApiError::from(CoreError::validation(
            "status is required (e.g. status=active, status=todo,in_progress, or status=all)",
        )));
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ApiError::from(CoreError::validation(
            "status is required (e.g. status=active, status=todo,in_progress, or status=all)",
        )));
    }
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok(None);
    }
    let mut out: Vec<Status> = Vec::new();
    for token in trimmed.split(',') {
        let t = token.trim();
        if t.is_empty() {
            continue;
        }
        if t.eq_ignore_ascii_case("active") {
            for s in [
                Status::Inbox,
                Status::Todo,
                Status::InProgress,
                Status::InReview,
            ] {
                if !out.contains(&s) {
                    out.push(s);
                }
            }
            continue;
        }
        let parsed = match t {
            "inbox" => Status::Inbox,
            "todo" => Status::Todo,
            "in_progress" => Status::InProgress,
            "in_review" => Status::InReview,
            "done" => Status::Done,
            "cancelled" => Status::Cancelled,
            other => {
                return Err(ApiError::from(CoreError::validation(format!(
                    "unknown task status: {other}"
                ))))
            }
        };
        if !out.contains(&parsed) {
            out.push(parsed);
        }
    }
    if out.is_empty() {
        return Err(ApiError::from(CoreError::validation(
            "status filter is empty after trimming",
        )));
    }
    Ok(Some(out))
}

async fn get_task(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;
    let task_id = id
        .parse::<TaskId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid task id: {id}"))))?;
    let task = state.tasks.get(task_id).await.map_err(ApiError::from)?;
    let t = task.ok_or_else(|| ApiError::from(CoreError::not_found(format!("task {id}"))))?;
    Ok(Json(t))
}

#[derive(Debug, Deserialize)]
struct HistoryEntityQuery {
    entity_type: String,
    entity_id: String,
    #[serde(default = "default_history_limit")]
    limit: u32,
}

#[derive(Debug, Deserialize)]
struct HistoryLatestQuery {
    #[serde(default = "default_history_limit")]
    limit: u32,
}

#[derive(Debug, Deserialize)]
struct HistoryCompareQuery {
    entity_type: String,
    entity_id: String,
    from: i64,
    to: i64,
}

fn default_history_limit() -> u32 {
    50
}

fn require_history_read(auth: &AuthContext, entity_type: &str) -> Result<(), ApiError> {
    let cap = match entity_type {
        "task" => Capability::TaskRead,
        "document" => Capability::DocumentRead,
        other => {
            return Err(ApiError::from(CoreError::validation(format!(
                "unknown version entity_type: {other}"
            ))))
        }
    };
    auth.require(cap).map_err(ApiError::from_missing_cap)
}

fn require_any_history_read(auth: &AuthContext) -> Result<(), ApiError> {
    let caps = auth.scope.capabilities;
    if caps.has(Capability::TaskRead) || caps.has(Capability::DocumentRead) {
        Ok(())
    } else {
        auth.require(Capability::TaskRead)
            .map_err(ApiError::from_missing_cap)
    }
}

/// `GET /v1/history?entity_type=task&entity_id=tsk_...` — immutable timeline
/// for one task or document, newest version first.
async fn list_entity_history(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<HistoryEntityQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_history_read(&auth, &q.entity_type)?;
    let versions = state
        .entity_versions
        .list_for_entity(&q.entity_type, &q.entity_id, q.limit)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(versions))
}

/// `GET /v1/history/{id}` — fetch one immutable version record.
async fn get_history_version(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_any_history_read(&auth)?;
    let version = state
        .entity_versions
        .get(&id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("version {id}"))))?;
    require_history_read(&auth, &version.entity_type)?;
    Ok(Json(version))
}

/// `GET /v1/history/compare?...&from=1&to=3` — compare two versions of the
/// same entity without mutating history.
async fn compare_history_versions(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<HistoryCompareQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_history_read(&auth, &q.entity_type)?;
    let from = state
        .entity_versions
        .get_by_number(&q.entity_type, &q.entity_id, q.from)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::from(CoreError::not_found(format!(
                "{} {} version {}",
                q.entity_type, q.entity_id, q.from
            )))
        })?;
    let to = state
        .entity_versions
        .get_by_number(&q.entity_type, &q.entity_id, q.to)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::from(CoreError::not_found(format!(
                "{} {} version {}",
                q.entity_type, q.entity_id, q.to
            )))
        })?;
    let diff = json!({
        "kind": "version_compare",
        "from_version": from.version_number,
        "to_version": to.version_number,
        "before": from.after,
        "after": to.after,
        "changed_fields": to.changed_fields,
        "diff": to.diff,
    });
    Ok(Json(json!({ "from": from, "to": to, "diff": diff })))
}

/// `GET /v1/history/latest` — latest task/document version records.
async fn latest_history(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<HistoryLatestQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_any_history_read(&auth)?;
    let versions = state
        .entity_versions
        .latest(q.limit)
        .await
        .map_err(ApiError::from)?;
    let caps = auth.scope.capabilities;
    let filtered: Vec<_> = versions
        .into_iter()
        .filter(|v| match v.entity_type.as_str() {
            "task" => caps.has(Capability::TaskRead),
            "document" => caps.has(Capability::DocumentRead),
            _ => false,
        })
        .collect();
    Ok(Json(filtered))
}

/// `GET /v1/history/summary` — compact agent-readable timeline.
async fn history_summary(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<HistoryEntityQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_history_read(&auth, &q.entity_type)?;
    let versions = state
        .entity_versions
        .list_for_entity(&q.entity_type, &q.entity_id, q.limit)
        .await
        .map_err(ApiError::from)?;
    let items: Vec<Value> = versions
        .into_iter()
        .map(|v| {
            json!({
                "version_id": v.id,
                "version_number": v.version_number,
                "event_type": v.event_type,
                "created_at": v.created_at,
                "summary": v.summary,
                "changed_fields": v.changed_fields,
            })
        })
        .collect();
    Ok(Json(json!({
        "entity_type": q.entity_type,
        "entity_id": q.entity_id,
        "items": items,
    })))
}

/// `POST /v1/history/{id}/rollback` — restore task/document state from an
/// existing version by creating normal new mutation events. Existing history is
/// never deleted or rewritten.
async fn rollback_history_version(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let version = state
        .entity_versions
        .get(&id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("version {id}"))))?;

    let after = version.after.clone().ok_or_else(|| {
        ApiError::from(CoreError::validation(format!(
            "version {id} has no after snapshot to roll back to"
        )))
    })?;

    let commands = match version.entity_type.as_str() {
        "task" => {
            auth.require(Capability::TaskWrite)
                .map_err(ApiError::from_missing_cap)?;
            vec![rollback_task_command(after)?]
        }
        "document" => {
            auth.require(Capability::DocumentWrite)
                .map_err(ApiError::from_missing_cap)?;
            rollback_document_commands(&state, after).await?
        }
        other => {
            return Err(ApiError::from(CoreError::validation(format!(
                "unknown version entity_type: {other}"
            ))))
        }
    };

    let mut all_events = Vec::new();
    for command in commands {
        let envs = state
            .commands
            .dispatch(command, actor_from(&auth, None))
            .await
            .map_err(ApiError::from)?;
        for env in &envs {
            state
                .entity_versions
                .mark_rollback(&env.id.to_string(), &version.id)
                .await
                .map_err(ApiError::from)?;
        }
        all_events.extend(envs);
    }

    let last = all_events.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: json!({
            "rollback_of_version_id": version.id,
            "events": all_events,
        }),
        warnings: vec![],
        client_command_id: None,
    }))
}

fn rollback_task_command(after: Value) -> Result<Command, ApiError> {
    let task: Task = serde_json::from_value(after)
        .map_err(|e| ApiError::from(CoreError::serde(e.to_string())))?;
    Ok(Command::UpdateTask {
        id: task.id,
        patch: TaskPatch {
            title: Some(task.title),
            description: Some(task.description),
            status: Some(task.status),
            priority: Some(task.priority),
            triage_state: Some(task.triage_state),
            due_at: Some(task.due_at),
            project_id: Some(task.project_id),
        },
    })
}

async fn rollback_document_commands(
    state: &AppState,
    after: Value,
) -> Result<Vec<Command>, ApiError> {
    let document: Document = serde_json::from_value(after)
        .map_err(|e| ApiError::from(CoreError::serde(e.to_string())))?;
    let current = state
        .documents
        .get(document.id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("document {}", document.id))))?;

    let mut commands = Vec::new();
    if current.title != document.title {
        commands.push(Command::RenameDocument {
            document_id: document.id,
            title: document.title,
        });
    }
    if current.content != document.content {
        commands.push(Command::ReplaceDocumentContent {
            document_id: document.id,
            content: document.content,
        });
    }
    Ok(commands)
}

async fn list_projects(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectRead)
        .map_err(ApiError::from_missing_cap)?;
    let projects = state.projects.list_all().await.map_err(ApiError::from)?;
    Ok(Json(projects))
}

#[derive(Debug, Serialize)]
struct LogicalWorkspaceSummary {
    id: String,
    name: String,
    roots: Vec<String>,
    project_count: i64,
}

#[derive(Debug, Deserialize)]
struct CreateLogicalWorkspaceBody {
    id: Option<String>,
    name: String,
    root_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MoveProjectWorkspaceBody {
    workspace_id: String,
    root_path: Option<String>,
}

async fn list_logical_workspaces(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectRead)
        .map_err(ApiError::from_missing_cap)?;
    let rows = sqlx::query(
        "SELECT t.id, t.name, COUNT(p.id) AS project_count \
         FROM tenants t \
         LEFT JOIN projects p ON p.tenant_id = t.id \
         GROUP BY t.id, t.name \
         ORDER BY t.created_at ASC",
    )
    .fetch_all(state.projects.pool())
    .await
    .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;

    let mut out = Vec::new();
    for row in rows {
        let id: String = row
            .try_get("id")
            .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
        let roots = sqlx::query_scalar(
            "SELECT root_path FROM workspace_roots WHERE tenant_id = ? ORDER BY root_path",
        )
        .bind(&id)
        .fetch_all(state.projects.pool())
        .await
        .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
        out.push(LogicalWorkspaceSummary {
            id,
            name: row
                .try_get("name")
                .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?,
            roots,
            project_count: row
                .try_get("project_count")
                .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?,
        });
    }

    Ok(Json(out))
}

async fn create_logical_workspace(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<CreateLogicalWorkspaceBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectWrite)
        .map_err(ApiError::from_missing_cap)?;
    let name = validate_workspace_name(&body.name)?;
    let id = body
        .id
        .as_deref()
        .map(validate_workspace_id)
        .transpose()?
        .unwrap_or_else(|| daruma_domain::slugify_title(&name));
    let now = chrono::Utc::now().to_rfc3339();

    let mut tx = state
        .projects
        .pool()
        .begin()
        .await
        .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
    sqlx::query(
        "INSERT INTO tenants (id, name, status, created_at, updated_at) \
         VALUES (?, ?, 'active', ?, ?) \
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, updated_at = excluded.updated_at",
    )
    .bind(&id)
    .bind(&name)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await
    .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
    if let Some(root_path) = clean_root_path(body.root_path.as_deref()) {
        upsert_workspace_root(&mut tx, &id, &root_path, &now).await?;
    }
    tx.commit()
        .await
        .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;

    Ok(Json(json!({ "id": id, "name": name })))
}

async fn move_project_to_workspace(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<MoveProjectWorkspaceBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectWrite)
        .map_err(ApiError::from_missing_cap)?;
    let project_id = id_str.parse::<ProjectId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid project id: {id_str}"
        )))
    })?;
    let workspace_id = validate_workspace_id(&body.workspace_id)?;
    let now = chrono::Utc::now().to_rfc3339();

    let mut tx = state
        .projects
        .pool()
        .begin()
        .await
        .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
    let tenant_exists: Option<String> = sqlx::query_scalar("SELECT id FROM tenants WHERE id = ?")
        .bind(&workspace_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
    if tenant_exists.is_none() {
        return Err(ApiError::from(CoreError::not_found(format!(
            "workspace {workspace_id}"
        ))));
    }
    let changed = sqlx::query("UPDATE projects SET tenant_id = ?, updated_at = ? WHERE id = ?")
        .bind(&workspace_id)
        .bind(&now)
        .bind(project_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?
        .rows_affected();
    if changed == 0 {
        return Err(ApiError::from(CoreError::not_found(format!(
            "project {project_id}"
        ))));
    }
    if let Some(root_path) = clean_root_path(body.root_path.as_deref()) {
        upsert_project_root(&mut tx, project_id, &root_path, &now).await?;
    }
    tx.commit()
        .await
        .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;

    let project = state
        .projects
        .get(project_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("project {project_id}"))))?;
    Ok(Json(project))
}

#[derive(Deserialize)]
struct CreateWorkUnitBody {
    work_unit: daruma_domain::NewWorkUnit,
}

/// `POST /v1/work-units` — create a work unit under a task (lazy
/// activation: only callers that opted into the work-unit layer use this;
/// plain tasks are untouched).
async fn create_work_unit(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<CreateWorkUnitBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskWrite)
        .map_err(ApiError::from_missing_cap)?;
    let envs = state
        .commands
        .dispatch(
            Command::CreateWorkUnit {
                work_unit: body.work_unit,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let unit = envs.iter().find_map(|e| match &e.payload {
        daruma_events::Event::WorkUnitCreated { work_unit } => Some(work_unit.clone()),
        _ => None,
    });
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "work_unit": unit }),
        warnings: vec![],
        client_command_id: None,
    }))
}

/// `GET /v1/tasks/{id}/work-units` — full decomposition state of a task.
async fn list_task_work_units(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;
    let task_id = id_str
        .parse::<TaskId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid task id: {id_str}"))))?;
    let units = state
        .work_units
        .list_by_task(task_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(units))
}

#[derive(Deserialize)]
struct WorkUnitDrainBody {
    task_id: TaskId,
    #[serde(default)]
    ttl_secs: Option<u32>,
}

/// `POST /v1/work-units/drain-next` — atomically claim the next
/// dispatchable work unit under a task and acquire its declared resource
/// leases (exclusive, P1). Concurrent callers each get a distinct unit.
///
/// Returns the dispatch briefing `{ work_unit, leases, acceptance }`, or
/// `{ work_unit: null }` when nothing is dispatchable, or
/// `{ work_unit: null, lease_conflict: {...} }` when the unit's declared
/// resources are held by another agent — the claim is reverted so the
/// lease holder (or anyone after release) can take the unit.
async fn work_unit_drain_next(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<WorkUnitDrainBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let agent_id = auth.agent_id;
    let ttl_secs = body.ttl_secs.unwrap_or(300);
    let ttl = chrono::Duration::seconds(ttl_secs as i64);

    let Some(unit) = state
        .work_units
        .try_claim_next(body.task_id, agent_id, ttl)
        .await
        .map_err(ApiError::from)?
    else {
        return Ok(Json(serde_json::json!({ "work_unit": null })));
    };

    // Atomically take the unit's declared exclusive resource leases. On
    // conflict the claim is reverted: the unit stays dispatchable for the
    // current lease holder.
    let mut leases = serde_json::Value::Null;
    if !unit.artifact_refs.is_empty() {
        let project_id = state
            .tasks
            .get(unit.task_id)
            .await
            .map_err(ApiError::from)?
            .and_then(|t| t.project_id);
        match state
            .work_leases
            .try_reserve_targets(
                agent_id,
                unit.task_id,
                project_id,
                unit.artifact_refs.clone(),
                daruma_domain::LeaseMode::Exclusive,
                ttl,
            )
            .await
            .map_err(ApiError::from)?
        {
            ReserveOutcome::Conflict {
                path,
                holder,
                holder_task,
            } => {
                state
                    .work_units
                    .revert_claim(unit.id, agent_id)
                    .await
                    .map_err(ApiError::from)?;
                return Ok(Json(serde_json::json!({
                    "work_unit": null,
                    "lease_conflict": {
                        "work_unit_id": unit.id,
                        "path": path,
                        "holder": holder,
                        "holder_task": holder_task,
                    },
                })));
            }
            ReserveOutcome::Reserved { leases: granted } => {
                leases = serde_json::json!(granted);
            }
        }
    }

    // Project the claim into the event log (audit + WS).
    let handler = state.commands.handler();
    let _ = handler
        .emit_system_event_as(
            actor_from(&auth, None),
            daruma_events::Event::WorkUnitClaimed {
                work_unit_id: unit.id,
                agent_id,
                expires_at: chrono::Utc::now() + ttl,
            },
        )
        .await;

    Ok(Json(serde_json::json!({
        "work_unit": unit,
        "leases": leases,
        "acceptance": unit.acceptance,
    })))
}

/// `GET /v1/agents/{agent_id}/capabilities` — the derived capability
/// profiles for an agent (P6). Advisory scheduling input, visible so the
/// preference is auditable.
async fn list_agent_capabilities(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(agent_id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;
    let agent_id = agent_id_str.parse::<AgentId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid agent id: {agent_id_str}"
        )))
    })?;
    let profiles = state
        .capability_profiles
        .list_for_agent(agent_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(profiles))
}

#[derive(Deserialize)]
struct PutCapabilityBody {
    capability: String,
    score: f64,
}

/// `PUT /v1/agents/{agent_id}/capabilities` — explicit human override
/// (`source = user_set`): mining never overwrites it, the staleness cutoff
/// does not apply. User override always wins (P6 invariant).
async fn put_agent_capability(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(agent_id_str): Path<String>,
    Json(body): Json<PutCapabilityBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectWrite)
        .map_err(ApiError::from_missing_cap)?;
    let agent_id = agent_id_str.parse::<AgentId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid agent id: {agent_id_str}"
        )))
    })?;
    if body.capability.trim().is_empty() {
        return Err(ApiError::from(CoreError::validation(
            "capability must not be empty",
        )));
    }
    if !(0.0..=1.0).contains(&body.score) {
        return Err(ApiError::from(CoreError::validation(
            "score must be within 0.0..=1.0",
        )));
    }
    state
        .capability_profiles
        .upsert_user_set(
            agent_id,
            body.capability.trim(),
            body.score,
            &daruma_shared::time::now().to_rfc3339(),
        )
        .await
        .map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({ "success": true })))
}

/// `DELETE /v1/agents/{agent_id}/capabilities/{capability}` — retract a
/// profile row (typically a user override; mining re-derives from future
/// evidence).
async fn delete_agent_capability(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path((agent_id_str, capability)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectWrite)
        .map_err(ApiError::from_missing_cap)?;
    let agent_id = agent_id_str.parse::<AgentId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid agent id: {agent_id_str}"
        )))
    })?;
    let removed = state
        .capability_profiles
        .delete(agent_id, &capability)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({ "success": true, "removed": removed })))
}

#[derive(Deserialize)]
struct RequestHandoffBody {
    handoff: daruma_domain::NewHandoffContract,
}

/// `POST /v1/handoffs` — request a handoff between two work units (P5).
/// Re-requesting an existing non-accepted pair reopens the same contract.
async fn request_handoff(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<RequestHandoffBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let envs = state
        .commands
        .dispatch(
            Command::RequestHandoff {
                handoff: body.handoff,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let contract = envs.iter().find_map(|e| match &e.payload {
        daruma_events::Event::HandoffRequested { handoff } => Some(handoff.clone()),
        _ => None,
    });
    let last = envs.last();
    Ok((
        StatusCode::CREATED,
        Json(MutationResponse {
            success: true,
            event_id: last.map(|e| e.id),
            event_seq: last.map(|e| e.seq),
            data: serde_json::json!({ "handoff": contract }),
            warnings: vec![],
            client_command_id: None,
        }),
    ))
}

#[derive(Deserialize, Default)]
struct AcceptHandoffBody {
    #[serde(default)]
    notes: Option<String>,
}

/// `POST /v1/handoffs/{id}/accept` — accept an open handoff; the consuming
/// unit becomes dispatchable again.
async fn accept_handoff(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    body: Option<Json<AcceptHandoffBody>>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let handoff_id = id_str.parse::<daruma_shared::HandoffId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!("invalid handoff id: {id_str}")))
    })?;
    let notes = body.and_then(|Json(b)| b.notes);
    let envs = state
        .commands
        .dispatch(
            Command::AcceptHandoff { handoff_id, notes },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "handoff_id": handoff_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

#[derive(Deserialize)]
struct RejectHandoffBody {
    reason: String,
    #[serde(default)]
    required_changes: Vec<String>,
}

/// `POST /v1/handoffs/{id}/reject` — reject an open handoff with a reason
/// and the changes required before a re-request.
async fn reject_handoff(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<RejectHandoffBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let handoff_id = id_str.parse::<daruma_shared::HandoffId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!("invalid handoff id: {id_str}")))
    })?;
    let envs = state
        .commands
        .dispatch(
            Command::RejectHandoff {
                handoff_id,
                reason: body.reason,
                required_changes: body.required_changes,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "handoff_id": handoff_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

/// `GET /v1/work-units/{id}/handoffs` — every handoff contract touching a
/// work unit (either side), newest first. The "handoff state visible"
/// surface: gate reasons stop being buried in comments.
async fn list_work_unit_handoffs(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;
    let id = id_str.parse::<daruma_shared::WorkUnitId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid work unit id: {id_str}"
        )))
    })?;
    let contracts = state
        .handoffs
        .list_for_work_unit(id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(contracts))
}

#[derive(Deserialize, Default)]
struct CompleteWorkUnitBody {
    #[serde(default)]
    outcome: Option<String>,
    /// Follow-up units the completer suggests dispatching next (advisory).
    #[serde(default)]
    next_suggested_units: Vec<daruma_shared::WorkUnitId>,
    #[serde(default)]
    produced_artifacts: Vec<String>,
}

/// `POST /v1/work-units/{id}/complete`
async fn complete_work_unit(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    body: Option<Json<CompleteWorkUnitBody>>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let id = parse_work_unit_id(&id_str)?;
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let envs = state
        .commands
        .dispatch(
            Command::CompleteWorkUnit {
                id,
                outcome: body.outcome,
                produced_artifacts: body.produced_artifacts,
                next_suggested_units: body.next_suggested_units,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "work_unit_id": id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

/// `POST /v1/work-units/{id}/release`
async fn release_work_unit(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let id = parse_work_unit_id(&id_str)?;
    let envs = state
        .commands
        .dispatch(Command::ReleaseWorkUnit { id }, actor_from(&auth, None))
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "work_unit_id": id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

fn parse_work_unit_id(id_str: &str) -> Result<daruma_shared::WorkUnitId, ApiError> {
    id_str.parse::<daruma_shared::WorkUnitId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid work unit id: {id_str}"
        )))
    })
}

#[derive(Deserialize)]
struct ResolveWorkspaceContextBody {
    /// Filesystem root the agent started in (absolute path).
    root_path: String,
    /// When true (default), an unknown root creates-or-binds a logical
    /// workspace and a default project; when false, resolve only.
    #[serde(default = "default_resolve_create")]
    create: bool,
    /// Bind the root into this existing workspace instead of deriving one.
    #[serde(default)]
    workspace_id: Option<String>,
}

fn default_resolve_create() -> bool {
    true
}

/// `POST /v1/workspace-registry/resolve` — map a filesystem root onto its
/// logical workspace + default project, creating both on first contact.
///
/// Resolution order: longest `project_roots` prefix → that project (+ its
/// workspace); else longest `workspace_roots` prefix → that workspace, with
/// a default project created and bound on demand; else (with `create`)
/// a new logical workspace named after the folder. Idempotent per root.
async fn resolve_workspace_context(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<ResolveWorkspaceContextBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectWrite)
        .map_err(ApiError::from_missing_cap)?;
    let root = body.root_path.trim().trim_end_matches('/').to_string();
    if root.is_empty() {
        return Err(ApiError::from(CoreError::validation(
            "root_path must not be empty",
        )));
    }
    let inside = |path: &str, base: &str| -> bool {
        path == base
            || path
                .strip_prefix(base)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    let pool = state.projects.pool();

    // 1. Longest project_roots prefix wins.
    let project_rows = sqlx::query("SELECT project_id, root_path FROM project_roots")
        .fetch_all(pool)
        .await
        .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
    let mut best: Option<(usize, String)> = None;
    for row in &project_rows {
        let pr: String = row
            .try_get("root_path")
            .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
        let base = pr.trim_end_matches('/');
        if inside(&root, base) && best.as_ref().is_none_or(|(len, _)| base.len() > *len) {
            let pid: String = row
                .try_get("project_id")
                .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
            best = Some((base.len(), pid));
        }
    }
    if let Some((_, project_id)) = best {
        let tenant: Option<String> =
            sqlx::query_scalar("SELECT tenant_id FROM projects WHERE id = ?")
                .bind(&project_id)
                .fetch_optional(pool)
                .await
                .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
        // DB stores display-prefixed ids; the JSON API serializes plain.
        let project_id = project_id.parse::<ProjectId>().map_err(|e| {
            ApiError::from(CoreError::storage(format!("bad project_roots id: {e}")))
        })?;
        return Ok(Json(json!({
            "resolved": true,
            "workspace_id": tenant,
            "project_id": project_id,
            "created_workspace": false,
            "created_project": false,
        })));
    }

    // 2. Longest workspace_roots prefix → existing logical workspace.
    let ws_rows = sqlx::query("SELECT tenant_id, root_path FROM workspace_roots")
        .fetch_all(pool)
        .await
        .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
    let mut ws_best: Option<(usize, String)> = None;
    for row in &ws_rows {
        let wr: String = row
            .try_get("root_path")
            .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
        let base = wr.trim_end_matches('/');
        if inside(&root, base) && ws_best.as_ref().is_none_or(|(len, _)| base.len() > *len) {
            let tid: String = row
                .try_get("tenant_id")
                .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
            ws_best = Some((base.len(), tid));
        }
    }
    let explicit_ws = body
        .workspace_id
        .as_deref()
        .map(validate_workspace_id)
        .transpose()?;
    let mut created_workspace = false;
    let had_explicit_ws = explicit_ws.is_some();
    let workspace_id = match explicit_ws.or(ws_best.map(|(_, t)| t)) {
        Some(t) => {
            let exists: Option<String> = sqlx::query_scalar("SELECT id FROM tenants WHERE id = ?")
                .bind(&t)
                .fetch_optional(pool)
                .await
                .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
            if exists.is_none() {
                return Err(ApiError::from(CoreError::not_found(format!(
                    "workspace {t}"
                ))));
            }
            t
        }
        None => {
            if !body.create {
                return Ok(Json(json!({ "resolved": false })));
            }
            let name = std::path::Path::new(&root)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("workspace")
                .to_string();
            let id = daruma_domain::slugify_title(&name);
            let id = validate_workspace_id(&id)?;
            let now = chrono::Utc::now().to_rfc3339();
            sqlx::query(
                "INSERT INTO tenants (id, name, status, created_at, updated_at) \
                 VALUES (?, ?, 'active', ?, ?) ON CONFLICT(id) DO NOTHING",
            )
            .bind(&id)
            .bind(&name)
            .bind(&now)
            .bind(&now)
            .execute(pool)
            .await
            .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
            created_workspace = true;
            id
        }
    };
    if !body.create {
        return Ok(Json(json!({
            "resolved": true,
            "workspace_id": workspace_id,
            "project_id": Value::Null,
            "created_workspace": false,
            "created_project": false,
        })));
    }

    // Bind the workspace root (idempotent) and create the default project.
    let now = chrono::Utc::now().to_rfc3339();
    {
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
        if created_workspace || had_explicit_ws {
            upsert_workspace_root(&mut tx, &workspace_id, &root, &now).await?;
        }
        tx.commit()
            .await
            .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
    }

    let title = std::path::Path::new(&root)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();
    let envs = state
        .commands
        .dispatch(
            Command::CreateProject {
                title: title.clone(),
                description: Some(format!("Auto-created for workspace root {root}")),
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let project_id = envs
        .iter()
        .find_map(|e| match &e.payload {
            daruma_events::Event::ProjectCreated { project } => Some(project.id),
            _ => None,
        })
        .ok_or_else(|| ApiError::from(CoreError::storage("project_created event missing")))?;

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
    sqlx::query("UPDATE projects SET tenant_id = ?, updated_at = ? WHERE id = ?")
        .bind(&workspace_id)
        .bind(&now)
        .bind(project_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
    upsert_project_root(&mut tx, project_id, &root, &now).await?;
    tx.commit()
        .await
        .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;

    Ok(Json(json!({
        "resolved": true,
        "workspace_id": workspace_id,
        "project_id": project_id,
        "created_workspace": created_workspace,
        "created_project": true,
    })))
}

/// `GET /v1/projects/{id}/settings` — per-project settings (auto-append
/// toggles, ON by default including for pre-migration projects).
async fn get_project_settings(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectRead)
        .map_err(ApiError::from_missing_cap)?;
    let id = parse_project_id(&id_str)?;
    state
        .projects
        .get(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("project {id}"))))?;
    let auto_append = state
        .project_settings
        .auto_append(id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "auto_append": auto_append })))
}

#[derive(Deserialize)]
struct ProjectSettingsPatchBody {
    #[serde(default)]
    auto_append: daruma_domain::AutoAppendPatch,
}

/// `PATCH /v1/projects/{id}/settings` — partial update of the auto-append
/// toggles, dispatched through the command bus (event-sourced).
async fn patch_project_settings(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<ProjectSettingsPatchBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectWrite)
        .map_err(ApiError::from_missing_cap)?;
    let id = parse_project_id(&id_str)?;
    let envs = state
        .commands
        .dispatch(
            Command::UpdateProjectSettings {
                project_id: id,
                auto_append: body.auto_append,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let auto_append = state
        .project_settings
        .auto_append(id)
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: json!({ "auto_append": auto_append }),
        warnings: vec![],
        client_command_id: None,
    }))
}

fn parse_project_id(id_str: &str) -> Result<ProjectId, ApiError> {
    id_str.parse::<ProjectId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid project id: {id_str}"
        )))
    })
}

// ── Lifecycle rules (docs/LIFECYCLE_RULES_SPEC.md §4) ───────────────────────────

/// Scope selector for listing rules. No params = tenant scope; otherwise the
/// most specific of project/plan/task wins. Mirrors the spec's nested-path
/// design as flat query params (Command-based, event-sourced under the hood).
#[derive(Deserialize)]
struct RuleScopeQuery {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    plan_id: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
}

impl RuleScopeQuery {
    fn into_scope(self) -> Result<daruma_domain::RuleScope, ApiError> {
        use daruma_domain::RuleScope;
        if let Some(t) = self.task_id {
            Ok(RuleScope::Task {
                id: t
                    .parse()
                    .map_err(|_| ApiError::from(CoreError::validation("invalid task id")))?,
            })
        } else if let Some(p) = self.plan_id {
            Ok(RuleScope::Plan {
                id: p
                    .parse()
                    .map_err(|_| ApiError::from(CoreError::validation("invalid plan id")))?,
            })
        } else if let Some(p) = self.project_id {
            Ok(RuleScope::Project {
                id: parse_project_id(&p)?,
            })
        } else {
            Ok(RuleScope::Tenant)
        }
    }
}

/// `GET /v1/rules` — list rules defined at a scope (tenant by default; pass
/// `project_id` / `plan_id` / `task_id` for a narrower scope).
async fn list_rules(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<RuleScopeQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectRead)
        .map_err(ApiError::from_missing_cap)?;
    let scope = q.into_scope()?;
    let rules = state
        .rules
        .list_for_scope(&scope)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "rules": rules })))
}

/// `GET /v1/rules/{id}` — fetch a single rule.
async fn get_rule(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectRead)
        .map_err(ApiError::from_missing_cap)?;
    let id = parse_rule_id(&id_str)?;
    let rule = state
        .rules
        .get(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("rule {id}"))))?;
    Ok(Json(json!({ "rule": rule })))
}

#[derive(Deserialize)]
struct CreateRuleBody {
    rule: daruma_domain::NewRule,
}

/// `POST /v1/rules` — create a rule (event-sourced via the command bus).
async fn create_rule(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<CreateRuleBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectWrite)
        .map_err(ApiError::from_missing_cap)?;
    let envs = state
        .commands
        .dispatch(
            Command::CreateRule { rule: body.rule },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let rule = match envs.last().map(|e| &e.payload) {
        Some(Event::RuleCreated { rule }) => Some(rule.clone()),
        _ => None,
    };
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: json!({ "rule": rule }),
        warnings: vec![],
        client_command_id: None,
    }))
}

#[derive(Deserialize)]
struct PatchRuleBody {
    #[serde(default, flatten)]
    patch: daruma_domain::RulePatch,
}

/// `PATCH /v1/rules/{id}` — partial update.
async fn patch_rule(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<PatchRuleBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectWrite)
        .map_err(ApiError::from_missing_cap)?;
    let id = parse_rule_id(&id_str)?;
    let envs = state
        .commands
        .dispatch(
            Command::UpdateRule {
                id,
                patch: body.patch,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let rule = state.rules.get(id).await.map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: json!({ "rule": rule }),
        warnings: vec![],
        client_command_id: None,
    }))
}

/// `DELETE /v1/rules/{id}` — disable a rule (`enabled=false`; not evaluated).
async fn disable_rule(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectWrite)
        .map_err(ApiError::from_missing_cap)?;
    let id = parse_rule_id(&id_str)?;
    let envs = state
        .commands
        .dispatch(Command::DisableRule { id }, actor_from(&auth, None))
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: json!({ "disabled": id.to_string() }),
        warnings: vec![],
        client_command_id: None,
    }))
}

fn parse_rule_id(id_str: &str) -> Result<RuleId, ApiError> {
    id_str
        .parse::<RuleId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid rule id: {id_str}"))))
}

// ── Evidence registry (OSS task 019eb65a-3185; spec §1.3) ───────────────────────

/// Query for listing evidence: scope selector plus `include_superseded`.
#[derive(Deserialize)]
struct EvidenceListQuery {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    plan_id: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    include_superseded: bool,
}

/// `GET /v1/evidence` — list evidence at a scope (tenant by default).
async fn list_evidence(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<EvidenceListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectRead)
        .map_err(ApiError::from_missing_cap)?;
    let scope = RuleScopeQuery {
        project_id: q.project_id,
        plan_id: q.plan_id,
        task_id: q.task_id,
    }
    .into_scope()?;
    let evidence = state
        .evidence
        .list_for_scope(&scope, q.include_superseded)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "evidence": evidence })))
}

/// `GET /v1/evidence/{id}` — fetch a single evidence record.
async fn get_evidence(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectRead)
        .map_err(ApiError::from_missing_cap)?;
    let id = id_str.parse::<EvidenceId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid evidence id: {id_str}"
        )))
    })?;
    let evidence = state
        .evidence
        .get(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("evidence {id}"))))?;
    Ok(Json(json!({ "evidence": evidence })))
}

#[derive(Deserialize)]
struct RecordEvidenceBody {
    evidence: daruma_domain::NewEvidence,
}

/// `POST /v1/evidence` — record evidence (event-sourced via the command bus).
async fn record_evidence(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<RecordEvidenceBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectWrite)
        .map_err(ApiError::from_missing_cap)?;
    let envs = state
        .commands
        .dispatch(
            Command::RecordEvidence {
                evidence: body.evidence,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let evidence = envs.iter().find_map(|e| match &e.payload {
        Event::EvidenceRecorded { evidence } => Some(evidence.clone()),
        _ => None,
    });
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: json!({ "evidence": evidence }),
        warnings: vec![],
        client_command_id: None,
    }))
}

// ── Audit primitives ────────────────────────────────────────────────────────

/// Parse a `FindingSeverity` from a query/body string, or a 400.
fn parse_severity(s: &str) -> Result<daruma_domain::FindingSeverity, ApiError> {
    daruma_domain::FindingSeverity::parse_str(s)
        .ok_or_else(|| ApiError::from(CoreError::validation(format!("invalid severity: {s}"))))
}

/// Parse a `FindingStatus` from a query/body string, or a 400.
fn parse_finding_status(s: &str) -> Result<daruma_domain::FindingStatus, ApiError> {
    daruma_domain::FindingStatus::parse_str(s)
        .ok_or_else(|| ApiError::from(CoreError::validation(format!("invalid status: {s}"))))
}

#[derive(Deserialize)]
struct FindingListQuery {
    project_id: String,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

/// `GET /v1/audit/findings?project_id=&severity=&category=&status=` — list
/// findings in a project, newest activity first. Read access (default profile).
async fn list_audit_findings(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<FindingListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectRead)
        .map_err(ApiError::from_missing_cap)?;
    let project_id = parse_project_id(&q.project_id)?;
    let filter = daruma_storage::FindingFilter {
        severity: q.severity.as_deref().map(parse_severity).transpose()?,
        category: q.category,
        status: q.status.as_deref().map(parse_finding_status).transpose()?,
    };
    let findings = state
        .audit_findings
        .list(project_id, &filter)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "findings": findings })))
}

/// `GET /v1/audit/findings/{id}` — fetch one finding.
async fn get_audit_finding(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectRead)
        .map_err(ApiError::from_missing_cap)?;
    let id = id_str
        .parse::<daruma_shared::AuditFindingId>()
        .map_err(|_| {
            ApiError::from(CoreError::validation(format!(
                "invalid finding id: {id_str}"
            )))
        })?;
    let finding = state
        .audit_findings
        .get(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("finding {id}"))))?;
    Ok(Json(json!({ "finding": finding })))
}

/// Wire shape for a recorded finding. Ids are accepted as strings and parsed
/// with `FromStr` (which strips the `prj_`/`tsk_`/… prefix) so callers pass the
/// natural prefixed ids every other endpoint returns — the typed `NewFinding`
/// fields are `#[serde(transparent)]` over bare UUIDs and would reject those.
#[derive(Deserialize)]
struct FindingInput {
    project_id: String,
    #[serde(default)]
    plan_id: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    document_id: Option<String>,
    #[serde(default)]
    artifact_id: Option<String>,
    check_key: String,
    category: String,
    severity: String,
    title: String,
    #[serde(default)]
    detail: String,
    #[serde(default)]
    remediation: String,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Deserialize)]
struct RecordFindingBody {
    finding: FindingInput,
}

/// Parse an optional prefixed id string into a typed id, or a 400.
fn parse_opt_id<T: std::str::FromStr>(
    raw: &Option<String>,
    label: &str,
) -> Result<Option<T>, ApiError> {
    raw.as_deref()
        .map(|s| {
            s.parse::<T>()
                .map_err(|_| ApiError::from(CoreError::validation(format!("invalid {label}: {s}"))))
        })
        .transpose()
}

/// `POST /v1/audit/findings` — record (upsert) a finding from a check. The
/// audit engine (Cloud-side) calls this; idempotent on the dedup key. Write
/// access.
async fn record_audit_finding(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<RecordFindingBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectWrite)
        .map_err(ApiError::from_missing_cap)?;
    let f = body.finding;
    let source = match f.source.as_deref() {
        Some(s) => daruma_domain::FindingSource::parse_str(s)
            .ok_or_else(|| ApiError::from(CoreError::validation(format!("invalid source: {s}"))))?,
        None => daruma_domain::FindingSource::Script,
    };
    let new = daruma_domain::NewFinding {
        project_id: parse_project_id(&f.project_id)?,
        entity: daruma_domain::FindingEntity {
            plan_id: parse_opt_id(&f.plan_id, "plan id")?,
            task_id: parse_opt_id(&f.task_id, "task id")?,
            document_id: parse_opt_id(&f.document_id, "document id")?,
            artifact_id: parse_opt_id(&f.artifact_id, "artifact id")?,
        },
        check_key: f.check_key,
        category: f.category,
        severity: parse_severity(&f.severity)?,
        title: f.title,
        detail: f.detail,
        remediation: f.remediation,
        source,
    };
    let id = state
        .audit_findings
        .upsert(&new)
        .await
        .map_err(ApiError::from)?;
    let finding = state.audit_findings.get(id).await.map_err(ApiError::from)?;
    Ok(Json(json!({ "finding": finding })))
}

#[derive(Deserialize)]
struct SetFindingStatusBody {
    status: String,
}

/// `POST /v1/audit/findings/{id}/status` — operator action: acknowledge / mute /
/// resolve / re-open a finding.
async fn set_audit_finding_status(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<SetFindingStatusBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectWrite)
        .map_err(ApiError::from_missing_cap)?;
    let id = id_str
        .parse::<daruma_shared::AuditFindingId>()
        .map_err(|_| {
            ApiError::from(CoreError::validation(format!(
                "invalid finding id: {id_str}"
            )))
        })?;
    let status = parse_finding_status(&body.status)?;
    let actor = daruma_domain::ActorRef::from_actor(&actor_from(&auth, None));
    let updated = state
        .audit_findings
        .set_status(id, status, &actor, daruma_shared::time::now())
        .await
        .map_err(ApiError::from)?;
    if !updated {
        return Err(ApiError::from(CoreError::not_found(format!(
            "finding {id}"
        ))));
    }
    Ok(Json(json!({ "success": true })))
}

#[derive(Deserialize)]
struct ResolveMissingBody {
    project_id: String,
    check_key: String,
    /// Finding ids re-seen in this run; everything else of `check_key` resolves.
    #[serde(default)]
    seen: Vec<String>,
}

/// `POST /v1/audit/findings/resolve-missing` — auto-resolve every still-open
/// finding of a `check_key` in a project that was not re-seen this run. The
/// audit engine calls this after a full check pass.
async fn resolve_missing_findings(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<ResolveMissingBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectWrite)
        .map_err(ApiError::from_missing_cap)?;
    let project_id = parse_project_id(&body.project_id)?;
    let seen = body
        .seen
        .iter()
        .map(|s| {
            s.parse::<daruma_shared::AuditFindingId>().map_err(|_| {
                ApiError::from(CoreError::validation(format!("invalid finding id: {s}")))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let actor = daruma_domain::ActorRef::from_actor(&actor_from(&auth, None));
    let resolved = state
        .audit_findings
        .resolve_missing(
            project_id,
            &body.check_key,
            &seen,
            &actor,
            daruma_shared::time::now(),
        )
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "resolved": resolved })))
}

#[derive(Deserialize)]
struct StuckTasksQuery {
    project_id: String,
    /// Status to inspect (default `in_progress`).
    #[serde(default)]
    status: Option<String>,
    /// Stuck threshold in hours (default 72).
    #[serde(default)]
    threshold_hours: Option<i64>,
}

/// `GET /v1/audit/heuristics/stuck-tasks` — tasks stuck in a status longer than
/// the threshold (Audit primitives task C·1). Read-only, no LLM.
async fn audit_stuck_tasks(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<StuckTasksQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;
    let project_id = parse_project_id(&q.project_id)?;
    let status = match q.status.as_deref() {
        Some(s) => Status::parse_str(s)
            .ok_or_else(|| ApiError::from(CoreError::validation(format!("invalid status: {s}"))))?,
        None => Status::InProgress,
    };
    let hours = q.threshold_hours.unwrap_or(72).max(0);
    let cutoff = daruma_shared::time::now() - chrono::Duration::hours(hours);
    let stuck = state
        .tasks
        .list_stuck_in_status(Some(project_id), status, cutoff)
        .await
        .map_err(ApiError::from)?;
    let items: Vec<_> = stuck
        .into_iter()
        .map(|s| {
            json!({
                "task_id": s.task.id,
                "title": s.task.title,
                "status": s.task.status.as_str(),
                "status_changed_at": s.status_changed_at.to_rfc3339(),
            })
        })
        .collect();
    Ok(Json(
        json!({ "status": status.as_str(), "threshold_hours": hours, "stuck": items }),
    ))
}

#[derive(Deserialize)]
struct DuplicateTasksQuery {
    project_id: String,
    /// bm25 threshold (pairs with rank <= this are emitted; default -1.0).
    #[serde(default)]
    threshold: Option<f64>,
    /// Per-task candidate cap (default 20).
    #[serde(default)]
    limit: Option<u32>,
}

/// `GET /v1/audit/heuristics/duplicate-tasks` — lexical duplicate-task
/// candidates, reusing the WorkspaceGraph FTS index (Audit primitives task C·2).
async fn audit_duplicate_tasks(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<DuplicateTasksQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;
    let project_id = parse_project_id(&q.project_id)?;
    let threshold = q.threshold.unwrap_or(-1.0);
    let limit = q.limit.unwrap_or(20).clamp(1, 200);
    let pairs = state
        .workspace_graph
        .duplicate_task_candidates(&project_id.to_string(), threshold, limit)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "candidates": pairs })))
}

#[derive(Deserialize)]
struct UnreadDocumentsQuery {
    project_id: String,
    /// Days since last read (default 30); documents never read always qualify.
    #[serde(default)]
    days: Option<i64>,
}

/// `GET /v1/audit/heuristics/unread-documents` — documents not read in N days
/// (Audit primitives task C·3), built on task A's read-tracking column.
async fn audit_unread_documents(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<UnreadDocumentsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::DocumentRead)
        .map_err(ApiError::from_missing_cap)?;
    let project_id = parse_project_id(&q.project_id)?;
    let days = q.days.unwrap_or(30).max(0);
    let cutoff = daruma_shared::time::now() - chrono::Duration::days(days);
    let docs = state
        .documents
        .list_unread_since(project_id, cutoff)
        .await
        .map_err(ApiError::from)?;
    let items: Vec<_> = docs
        .into_iter()
        .map(|d| {
            json!({
                "document_id": d.id,
                "title": d.title,
                "kind": d.kind.as_str(),
                "last_read_at": d.last_read_at.map(|t| t.to_rfc3339()),
                "read_count": d.read_count,
            })
        })
        .collect();
    Ok(Json(json!({ "days": days, "unread": items })))
}

fn validate_workspace_name(name: &str) -> Result<String, ApiError> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > 120 {
        return Err(ApiError::from(CoreError::validation(
            "workspace name must be 1..=120 characters",
        )));
    }
    Ok(trimmed.to_string())
}

fn validate_workspace_id(id: &str) -> Result<String, ApiError> {
    let id = id.trim();
    if id.is_empty()
        || id.len() > 80
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::from(CoreError::validation(
            "workspace_id must be 1..=80 chars: [A-Za-z0-9_-]",
        )));
    }
    Ok(id.to_string())
}

fn clean_root_path(path: Option<&str>) -> Option<String> {
    let path = path?.trim();
    (!path.is_empty()).then(|| path.to_string())
}

async fn upsert_workspace_root(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    workspace_id: &str,
    root_path: &str,
    now: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO workspace_roots (id, tenant_id, root_path, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(root_path) DO UPDATE SET tenant_id = excluded.tenant_id, updated_at = excluded.updated_at",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(workspace_id)
    .bind(root_path)
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
    Ok(())
}

async fn upsert_project_root(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    project_id: ProjectId,
    root_path: &str,
    now: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO project_roots (id, project_id, root_path, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(root_path) DO UPDATE SET project_id = excluded.project_id, updated_at = excluded.updated_at",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(project_id.to_string())
    .bind(root_path)
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(|e| ApiError::from(CoreError::storage(e.to_string())))?;
    Ok(())
}

#[derive(Deserialize)]
struct ProjectTriagePatch {
    triage_enabled: bool,
}

async fn patch_project_triage(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<ProjectTriagePatch>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectWrite)
        .map_err(ApiError::from_missing_cap)?;
    let id = id_str.parse::<daruma_shared::ProjectId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid project id: {id_str}"
        )))
    })?;
    let project = state
        .projects
        .set_triage_enabled(id, body.triage_enabled)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("project {id}"))))?;
    Ok(Json(project))
}

async fn list_project_triage(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;
    let id = id_str.parse::<daruma_shared::ProjectId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid project id: {id_str}"
        )))
    })?;
    let project = state
        .projects
        .get(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("project {id}"))))?;
    if !project.triage_enabled {
        return Ok(Json(Vec::<Task>::new()));
    }
    let tasks = state
        .tasks
        .list_triage_queue(id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(tasks))
}

#[derive(Deserialize)]
struct TaskTriagePatch {
    triage_state: Option<TriageState>,
}

async fn patch_task_triage(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<TaskTriagePatch>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskWrite)
        .map_err(ApiError::from_missing_cap)?;
    let id = id_str
        .parse::<TaskId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid task id: {id_str}"))))?;
    let task = state
        .tasks
        .set_triage_state(id, body.triage_state)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("task {id}"))))?;
    Ok(Json(task))
}

/// `DELETE /v1/projects/{id}` — delete a project, but only when it is empty.
///
/// Returns `409 Conflict` with `{tasks_count, plans_count}` when the project
/// still contains tasks or plans.  Dispatches `Command::DeleteProject` on
/// success so the projection update goes through the regular event log.
async fn delete_project(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::ProjectWrite)
        .map_err(ApiError::from_missing_cap)?;

    let id = id_str.parse::<daruma_shared::ProjectId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid project id: {id_str}"
        )))
    })?;

    // Verify the project exists; 404 if not.
    if state
        .projects
        .get(id)
        .await
        .map_err(ApiError::from)?
        .is_none()
    {
        return Err(ApiError::from(CoreError::not_found(format!(
            "project {id}"
        ))));
    }

    // Emptiness invariants — fail fast with a structured 409 payload.
    let tasks_count = state
        .tasks
        .list_by_project(Some(id))
        .await
        .map_err(ApiError::from)?
        .len();
    let plans_count = state
        .plans
        .list_by_project(id, None)
        .await
        .map_err(ApiError::from)?
        .len();
    if tasks_count > 0 || plans_count > 0 {
        return Err(ApiError::from(CoreError::conflict(format!(
            "project_not_empty: tasks={tasks_count}, plans={plans_count}"
        ))));
    }

    let envs = state
        .commands
        .dispatch(
            daruma_api_dto::Command::DeleteProject { id },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;

    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "project_id": id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

async fn dispatch_command(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(envelope): Json<CommandEnvelope>,
) -> Result<impl IntoResponse, ApiError> {
    // Map the command to its required capability and gate before dispatch.
    let needed = capability_for_command(&envelope.command);
    auth.require(needed).map_err(ApiError::from_missing_cap)?;

    // Idempotency check (Linear A.1) — return cached result for seen commands.
    if let Some(ccid) = envelope.client_command_id {
        if let Some((eid, eseq)) = state
            .idempotency
            .lookup(ccid)
            .await
            .map_err(ApiError::from)?
        {
            let data = load_cached_event_data(&state, eid, eseq)
                .await?
                .unwrap_or(serde_json::Value::Null);
            return Ok(Json(MutationResponse {
                success: true,
                event_id: Some(eid),
                event_seq: Some(eseq),
                data,
                warnings: vec![],
                client_command_id: Some(ccid),
            }));
        }
    }

    let mut warnings = mutation_warnings(&state, &envelope.command)
        .await
        .map_err(ApiError::from)?;

    // Lifecycle-gate warnings (docs/LIFECYCLE_RULES_SPEC.md §1.5) ride the
    // same `warnings` channel; blocked checks surface as a Conflict error
    // ("rule_blocked: …") from dispatch itself.
    let outcome = state
        .commands
        .dispatch_with_warnings(envelope.command, envelope.actor)
        .await
        .map_err(ApiError::from)?;
    warnings.extend(outcome.warnings);
    let envelopes = outcome.events;

    // Persist idempotency record for future retries.
    if let Some(ccid) = envelope.client_command_id {
        if let Some(last) = envelopes.last() {
            state
                .idempotency
                .insert(ccid, last.id, last.seq)
                .await
                .map_err(ApiError::from)?;
        }
    }

    let last = envelopes.last();
    let data = serde_json::to_value(&envelopes).unwrap_or(serde_json::Value::Null);
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data,
        warnings,
        client_command_id: envelope.client_command_id,
    }))
}

#[derive(serde::Deserialize, Default)]
struct McpHttpQuery {
    /// Tool surface profile override: `default` or `full`.
    /// Falls back to DARUMA_MCP_PROFILE, then `default`.
    profile: Option<String>,
}

async fn mcp_http(
    headers: HeaderMap,
    Query(query): Query<McpHttpQuery>,
    Json(request): Json<JsonRpcRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let profile = match query.profile.as_deref() {
        Some(raw) => daruma_mcp::ToolProfile::parse(raw).ok_or_else(|| {
            ApiError::from(CoreError::validation(format!(
                "unknown MCP profile `{raw}` — expected `default` or `full`"
            )))
        })?,
        None => daruma_mcp::ToolProfile::from_env(),
    };
    let token = bearer_token(&headers)?;
    let http = reqwest::Client::builder()
        .user_agent(format!(
            "daruma-server-mcp/{}",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .map_err(|e| ApiError::from(CoreError::validation(e.to_string())))?;
    let mut client = ApiClient::with_http(mcp_base_url(&headers)?, token, http);
    if let Some(workspace_id) = header_string(&headers, "x-daruma-workspace-id") {
        client = client.with_workspace_id(workspace_id);
    }

    match dispatch_mcp_request(&client, profile, request).await {
        Some(response) => Ok(Json(response).into_response()),
        None => Ok(StatusCode::ACCEPTED.into_response()),
    }
}

fn bearer_token(headers: &HeaderMap) -> Result<String, ApiError> {
    let Some(auth) = header_string(headers, "authorization") else {
        return Err(ApiError::from(CoreError::unauthorized(
            "missing Authorization bearer token",
        )));
    };
    let Some(token) = auth.strip_prefix("Bearer ") else {
        return Err(ApiError::from(CoreError::unauthorized(
            "missing Authorization bearer token",
        )));
    };
    let token = token.trim();
    if token.is_empty() {
        return Err(ApiError::from(CoreError::unauthorized(
            "missing Authorization bearer token",
        )));
    }
    Ok(token.to_string())
}

fn mcp_base_url(headers: &HeaderMap) -> Result<String, ApiError> {
    let host = header_string(headers, "x-forwarded-host")
        .or_else(|| header_string(headers, "host"))
        .ok_or_else(|| ApiError::from(CoreError::validation("missing Host header")))?;
    let proto = header_string(headers, "x-forwarded-proto").unwrap_or_else(|| "http".into());
    Ok(format!("{}://{}", proto.trim_end_matches('/'), host))
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

async fn load_cached_event_data(
    state: &AppState,
    event_id: daruma_shared::EventId,
    event_seq: u64,
) -> Result<Option<serde_json::Value>, ApiError> {
    let events = state
        .store
        .load_since(event_seq.saturating_sub(1), 1)
        .await
        .map_err(ApiError::from)?;
    let Some(event) = events.into_iter().find(|event| event.id == event_id) else {
        return Ok(None);
    };
    Ok(Some(serde_json::json!([event])))
}

async fn mutation_warnings(
    state: &AppState,
    command: &Command,
) -> Result<Vec<MutationWarning>, CoreError> {
    let Command::SetStatus {
        id,
        status: Status::InProgress,
        force: false,
    } = command
    else {
        return Ok(vec![]);
    };

    let readiness = plan_readiness::can_start(&state.tasks, &state.relations, *id).await?;
    if readiness.ready {
        return Ok(vec![]);
    }

    Ok(vec![MutationWarning {
        code: "task_blocked".to_string(),
        message: format!(
            "task {id} has {} active blocker(s); pass force=true to acknowledge",
            readiness.blockers.len()
        ),
        details: json!({
            "task_id": id,
            "ready": readiness.ready,
            "reason": readiness.reason,
            "blockers": readiness.blockers,
        }),
    }])
}

#[derive(Deserialize)]
struct EventsQuery {
    /// Return events with seq > since (default: 0).
    #[serde(default)]
    since: u64,
    /// Maximum events to return (default: 100, cap: 1000).
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    100
}

async fn list_events(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<EventsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;
    let limit = q.limit.min(1000);
    let events = state
        .store
        .load_since(q.since, limit)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(events))
}

#[derive(Deserialize)]
struct ReplicaEventsBody {
    events: Vec<EventEnvelope>,
}

async fn append_replica_events(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<ReplicaEventsBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskWrite)
        .map_err(ApiError::from_missing_cap)?;

    if body.events.is_empty() {
        return Err(ApiError::from(CoreError::validation(
            "events must not be empty",
        )));
    }
    if body.events.len() > 1000 {
        return Err(ApiError::from(CoreError::validation(
            "events batch must contain at most 1000 events",
        )));
    }

    let mut accepted = Vec::with_capacity(body.events.len());
    let mut duplicate_count = 0usize;
    for mut envelope in body.events {
        envelope.seq = 0;
        if let Some(existing) = state
            .store
            .load_by_id(envelope.id)
            .await
            .map_err(ApiError::from)?
        {
            accepted.push(existing);
            duplicate_count += 1;
            continue;
        }

        let persisted = state.store.append(envelope).await.map_err(ApiError::from)?;
        apply_persisted_event(&state, &persisted).await?;
        state.hub.bus.publish(persisted.clone());
        accepted.push(persisted);
    }

    let last = accepted.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: json!({
            "events": accepted,
            "duplicates": duplicate_count,
        }),
        warnings: vec![],
        client_command_id: None,
    }))
}

async fn apply_persisted_event(state: &AppState, env: &EventEnvelope) -> Result<(), ApiError> {
    state.tasks.apply_event(env).await.map_err(ApiError::from)?;
    state
        .projects
        .apply_event(env)
        .await
        .map_err(ApiError::from)?;
    state
        .comments
        .apply_event(env)
        .await
        .map_err(ApiError::from)?;
    state
        .activity
        .apply_event(env)
        .await
        .map_err(ApiError::from)?;
    state.relations.apply_event(&env.payload).await;
    state.plans.apply_event(env).await.map_err(ApiError::from)?;
    state.runs.apply_event(env).await.map_err(ApiError::from)?;
    state
        .run_notes
        .apply_event(env)
        .await
        .map_err(ApiError::from)?;
    state
        .sessions
        .apply_event(env)
        .await
        .map_err(ApiError::from)?;
    state
        .claims
        .apply_event(env)
        .await
        .map_err(ApiError::from)?;
    state
        .external_refs
        .apply_event(env)
        .await
        .map_err(ApiError::from)?;
    state
        .documents
        .apply_event(env)
        .await
        .map_err(ApiError::from)?;
    Ok(())
}

/// `POST /v1/ai/analyze-complexity/{plan_id}` — §3.8.3.
///
/// Loads the plan's task list, builds a `TaskBrief` per task, hands them to
/// `daruma_ai::analyze_complexity_batch` as **one** LLM call, and upserts
/// the resulting hints into the `task_complexity_hints` projection. The
/// response surfaces `batch_id` + the freshly written hints so callers (e.g.
/// the §3.8.4 hint-aware `daruma_ai_decompose`) can chain immediately.
///
/// **Deprecated delegation-shim.** The complexity *analysis* (raw tasks →
/// scores/hints) is planning-layer logic whose canonical home is
/// `yatagarasu::analyze_complexity_batch` (`planning_oss`). This route is
/// retained unchanged until the cloud cutover wires the call to the
/// planning layer (separate plan); only the **write-back** half — building
/// `TaskBrief`s and upserting `task_complexity_hints` — is structural
/// execution work that stays in core regardless. The standalone write-back
/// contract already exists as `POST /v1/complexity-hints`
/// ([`upsert_complexity_hints`]): planning analyses, core persists.
/// Optional body for `POST /v1/ai/analyze-complexity/{plan_id}`.
///
/// Pre-§3.8.13 callers send no body (or `{}`); the new
/// `use_research_provider` flag is opt-in and silently ignored until
/// the §3.8.9 provider abstraction lands.
#[derive(Deserialize, Default)]
#[serde(default)]
struct AiAnalyzeComplexityBody {
    #[allow(dead_code)]
    use_research_provider: Option<bool>,
}

async fn ai_analyze_complexity(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(plan_id_str): Path<String>,
    _body: Option<Json<AiAnalyzeComplexityBody>>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::AgentDispatch)
        .map_err(ApiError::from_missing_cap)?;

    let plan_id = plan_id_str.parse::<PlanId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid plan id: {plan_id_str}"
        )))
    })?;

    // Existence check — surface 404 rather than silently returning [].
    let _plan = state
        .plans
        .get(plan_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("plan {plan_id_str}"))))?;

    let client = state.ai.as_ref().ok_or_else(|| {
        ApiError::from(CoreError::ai("AI not configured (OPENAI_API_KEY not set)"))
    })?;

    // Collect TaskBrief inputs in plan-position order.
    let plan_tasks = state
        .plans
        .list_tasks_ordered(plan_id)
        .await
        .map_err(ApiError::from)?;

    let mut briefs: Vec<daruma_domain::TaskBrief> = Vec::with_capacity(plan_tasks.len());
    for pt in &plan_tasks {
        let Some(task) = state.tasks.get(pt.task_id).await.map_err(ApiError::from)? else {
            // Plan references a task that no longer exists — skip rather
            // than 500. The projection won't get a row, the response will
            // omit it, callers see the gap.
            continue;
        };
        briefs.push(daruma_domain::TaskBrief {
            task_id: task.id,
            title: task.title,
            description: task.description,
        });
    }

    if briefs.is_empty() {
        return Ok(Json(serde_json::json!({
            "plan_id": plan_id.to_string(),
            "batch_id": null,
            "hints": [],
        })));
    }

    // §3.8.12: push-based progress on Channel::AiOps. Best-effort.
    let handler = state.commands.handler();
    let op_id = daruma_shared::AiOpId::new();
    let _ = handler
        .emit_system_event(daruma_events::Event::AiOperationStarted {
            op_id,
            kind: "analyze_complexity".into(),
            target_id: plan_id.to_string(),
            at: chrono::Utc::now(),
        })
        .await;
    let _ = handler
        .emit_system_event(daruma_events::Event::AiOperationPhaseChanged {
            op_id,
            phase: "llm_call".into(),
            detail: Some(format!("{} task(s)", briefs.len())),
            at: chrono::Utc::now(),
        })
        .await;
    let result = daruma_ai::analyze_complexity_batch(client, briefs).await;
    let outcome = match &result {
        Ok(_) => "ok".to_string(),
        Err(e) => format!("error: {e}"),
    };
    let hints = match result {
        Ok(h) => h,
        Err(e) => {
            let _ = handler
                .emit_system_event(daruma_events::Event::AiOperationCompleted {
                    op_id,
                    outcome,
                    at: chrono::Utc::now(),
                })
                .await;
            return Err(ApiError::from(e));
        }
    };

    let _ = handler
        .emit_system_event(daruma_events::Event::AiOperationPhaseChanged {
            op_id,
            phase: "apply".into(),
            detail: None,
            at: chrono::Utc::now(),
        })
        .await;
    state
        .complexity_hints
        .upsert_batch(&hints)
        .await
        .map_err(ApiError::from)?;
    let _ = handler
        .emit_system_event(daruma_events::Event::AiOperationCompleted {
            op_id,
            outcome,
            at: chrono::Utc::now(),
        })
        .await;

    let batch_id = hints.first().map(|h| h.batch_id.clone());
    Ok(Json(serde_json::json!({
        "plan_id": plan_id.to_string(),
        "batch_id": batch_id,
        "hints": hints,
    })))
}

/// One complexity hint draft as posted by the planning layer. Mirrors
/// `yatagarasu::ComplexityHintDraft`: deliberately free of `batch_id` /
/// `generated_at` — persistence identity is assigned here, by core.
#[derive(Deserialize)]
struct ComplexityHintDraftBody {
    task_id: TaskId,
    score: u8,
    #[serde(default)]
    recommended_subtasks: u8,
    #[serde(default)]
    expansion_hint: String,
    #[serde(default)]
    reasoning: String,
}

#[derive(Deserialize)]
struct UpsertComplexityHintsBody {
    hints: Vec<ComplexityHintDraftBody>,
}

/// `POST /v1/complexity-hints` — the projection write-back contract of the
/// execution/planning layer boundary.
///
/// The planning layer (`yatagarasu::analyze_complexity_batch`) returns pure
/// [`ComplexityHintDraftBody`]-shaped drafts and **never writes to storage**;
/// its caller posts them here, and core assigns `batch_id` + `generated_at`
/// and upserts the `task_complexity_hints` projection. This is the same
/// write-back half the deprecated `/ai/analyze-complexity/{plan_id}` shim
/// performs internally today; at the cloud cutover callers switch to
/// planning-layer analysis + this endpoint and the shim is removed.
async fn upsert_complexity_hints(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<UpsertComplexityHintsBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::AgentDispatch)
        .map_err(ApiError::from_missing_cap)?;

    if body.hints.is_empty() {
        return Err(ApiError::from(CoreError::validation(
            "hints must not be empty",
        )));
    }
    // Mirrors the planning layer's per-analysis cap: one batch analysis
    // yields at most MAX_BATCH_TASKS drafts, so a larger write-back is a
    // caller bug, not a bigger workload.
    if body.hints.len() > daruma_ai::MAX_BATCH_TASKS {
        return Err(ApiError::from(CoreError::validation(format!(
            "too many hints: {} (max {})",
            body.hints.len(),
            daruma_ai::MAX_BATCH_TASKS
        ))));
    }

    // Trust boundary: reject unknown task ids instead of silently writing
    // orphan projection rows.
    let mut unknown = Vec::new();
    for draft in &body.hints {
        if state
            .tasks
            .get(draft.task_id)
            .await
            .map_err(ApiError::from)?
            .is_none()
        {
            unknown.push(draft.task_id.to_string());
        }
    }
    if !unknown.is_empty() {
        return Err(ApiError::from(CoreError::validation(format!(
            "unknown task ids: {}",
            unknown.join(", ")
        ))));
    }

    let batch_id = uuid::Uuid::now_v7().to_string();
    let generated_at = daruma_shared::time::now();
    let hints: Vec<ComplexityHint> = body
        .hints
        .into_iter()
        .map(|draft| ComplexityHint {
            task_id: draft.task_id,
            // Same clamps the analysis applies at parse time — re-applied at
            // the trust boundary so a buggy caller cannot skew the projection.
            score: draft.score.clamp(1, 10),
            recommended_subtasks: draft.recommended_subtasks.min(20),
            expansion_hint: draft.expansion_hint,
            reasoning: draft.reasoning,
            generated_at,
            batch_id: batch_id.clone(),
        })
        .collect();

    state
        .complexity_hints
        .upsert_batch(&hints)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(json!({
        "batch_id": batch_id,
        "hints": hints,
    })))
}

// ── Activity handlers ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ActivityQuery {
    /// Exclusive lower bound on `seq` (cursor from previous page).
    #[serde(default)]
    cursor: Option<u64>,
    /// Maximum rows to return (default: 100, cap: 500).
    #[serde(default)]
    limit: Option<u32>,
    /// Comma-separated list of verbs to include (e.g. `closed,commented`).
    #[serde(default)]
    verbs: Option<String>,
}

async fn list_task_activity(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(task_id_str): Path<String>,
    Query(params): Query<ActivityQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;

    let task_id = task_id_str.parse::<TaskId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid task id: {task_id_str}"
        )))
    })?;

    // Resolve project_id from the live tasks projection; fall back to the
    // activity audit trail for deleted tasks (hard-deleted from tasks table).
    let project_id = match state.tasks.get(task_id).await.map_err(ApiError::from)? {
        Some(t) => t.project_id,
        None => {
            let (probe, _, _) = state
                .activity
                .list_for_task(task_id, None, 1, None)
                .await
                .map_err(ApiError::from)?;
            match probe.into_iter().next() {
                Some(act) => act.project_id,
                None => {
                    return Err(ApiError::from(CoreError::not_found(format!(
                        "task {task_id_str}"
                    ))))
                }
            }
        }
    };

    // Project-scope gate.
    if !auth.scope.projects.allows(project_id) {
        return Err(ApiError::from(CoreError::forbidden(
            "project access denied",
        )));
    }

    // Parse optional verb CSV filter; any unknown verb → 400.
    let verb_filter: Option<Vec<Verb>> = params
        .verbs
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| {
                    s.parse::<Verb>().map_err(|_| {
                        ApiError::from(CoreError::validation(format!("unknown verb: {s}")))
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;

    let limit = params.limit.unwrap_or(100).min(500);

    let (items, next_cursor, has_more) = state
        .activity
        .list_for_task(task_id, params.cursor, limit, verb_filter.as_deref())
        .await
        .map_err(ApiError::from)?;

    Ok(Json(json!({
        "items": items,
        "next_cursor": next_cursor,
        "has_more": has_more,
    })))
}

// ── Comment handlers ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AddCommentBody {
    #[serde(default)]
    id: Option<CommentId>,
    body: String,
    #[serde(default)]
    parent_id: Option<CommentId>,
    /// Optional semantic classification (§3.8.8). Accepted as either
    /// the canonical snake_case form (`"research"`) or the PascalCase
    /// variant name (`"Research"`). Unknown values yield HTTP 400.
    #[serde(default)]
    kind: Option<String>,
}

async fn add_comment(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(task_id_str): Path<String>,
    Json(body): Json<AddCommentBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::CommentWrite)
        .map_err(ApiError::from_missing_cap)?;
    let task_id = task_id_str.parse::<TaskId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid task id: {task_id_str}"
        )))
    })?;
    let kind = body
        .kind
        .as_deref()
        .map(|s| s.parse::<CommentKind>())
        .transpose()
        .map_err(|e| ApiError::from(CoreError::validation(e)))?;
    let new_comment = NewComment {
        id: body.id,
        task_id,
        body: body.body,
        parent_id: body.parent_id,
        kind,
    };
    let envs = state
        .commands
        .dispatch(
            Command::AddComment {
                comment: new_comment,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let comment = envs
        .into_iter()
        .find_map(|e| {
            if let Event::CommentAdded { comment } = e.payload {
                Some(comment)
            } else {
                None
            }
        })
        .ok_or_else(|| ApiError::from(CoreError::storage("expected CommentAdded event")))?;
    Ok((StatusCode::CREATED, Json(comment)))
}

async fn list_task_comments(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(task_id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::CommentRead)
        .map_err(ApiError::from_missing_cap)?;
    let task_id = task_id_str.parse::<TaskId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid task id: {task_id_str}"
        )))
    })?;
    let comments = state
        .comments
        .list_for_task(task_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(comments))
}

async fn edit_comment(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(patch): Json<CommentPatch>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::CommentWrite)
        .map_err(ApiError::from_missing_cap)?;
    let comment_id = id_str.parse::<CommentId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid comment id: {id_str}"
        )))
    })?;
    state
        .commands
        .dispatch(
            Command::EditComment {
                id: comment_id,
                patch,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let updated = state
        .comments
        .get(comment_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("comment {comment_id}"))))?;
    Ok(Json(updated))
}

async fn delete_comment(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::CommentWrite)
        .map_err(ApiError::from_missing_cap)?;
    let comment_id = id_str.parse::<CommentId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid comment id: {id_str}"
        )))
    })?;
    state
        .commands
        .dispatch(
            Command::DeleteComment { id: comment_id },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Token admin handlers ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateTokenBody {
    kind: TokenKind,
    agent_id: AgentId,
    #[serde(default)]
    projects: Option<ProjectFilter>,
    /// Bit-encoded capability mask. Default: empty mask (read-only-by-omission).
    #[serde(default)]
    capabilities: Capabilities,
    #[serde(default = "default_rate_limit")]
    rate_limit_per_min: u32,
    #[serde(default)]
    expires_in_days: Option<i64>,
}

fn default_rate_limit() -> u32 {
    60
}

#[derive(Serialize)]
struct CreatedTokenResponse {
    /// Plaintext token — returned **only** on creation, never again.
    secret: String,
    token: daruma_auth::ApiToken,
}

async fn create_token(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<CreateTokenBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TokenWrite)
        .map_err(ApiError::from_missing_cap)?;

    let expired_at = body
        .expires_in_days
        .map(|days| daruma_shared::time::now() + chrono::Duration::days(days));

    let spec = NewTokenSpec {
        kind: body.kind,
        agent_id: body.agent_id,
        scope: TokenScope {
            projects: body.projects.unwrap_or(ProjectFilter::All),
            capabilities: body.capabilities,
        },
        rate_limit_per_min: body.rate_limit_per_min,
        expired_at,
    };

    let secret = generate(spec).map_err(ApiError::from)?;
    state
        .tokens
        .insert(secret.record.clone())
        .await
        .map_err(ApiError::from)?;

    Ok((
        StatusCode::CREATED,
        Json(CreatedTokenResponse {
            secret: secret.plaintext,
            token: secret.record,
        }),
    ))
}

#[derive(Deserialize)]
struct ListTokensQuery {
    #[serde(default)]
    agent_id: Option<AgentId>,
}

async fn list_tokens(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<ListTokensQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TokenRead)
        .map_err(ApiError::from_missing_cap)?;

    let tokens = match q.agent_id {
        Some(id) => state
            .tokens
            .list_for_agent(id)
            .await
            .map_err(ApiError::from)?,
        None => {
            // No filter — only admin tokens may see all.
            auth.require(Capability::Admin)
                .map_err(ApiError::from_missing_cap)?;
            state
                .tokens
                .list_for_agent(auth.agent_id)
                .await
                .map_err(ApiError::from)?
        }
    };
    Ok(Json(tokens))
}

async fn revoke_token(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TokenWrite)
        .map_err(ApiError::from_missing_cap)?;
    let id = id_str.parse::<TokenId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!("invalid token id: {id_str}")))
    })?;
    let revoked = state.tokens.revoke(id).await.map_err(ApiError::from)?;
    if revoked {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::from(CoreError::not_found(format!(
            "token {id_str}"
        ))))
    }
}

// ── Agent inbox handlers ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct InboxQuery {
    /// Return events with `seq > since` (default: the cursor stored in
    /// `agent_acks`, or 0 if absent).
    #[serde(default)]
    since: Option<u64>,
    /// Max events per response (cap 1000; default 100).
    #[serde(default = "default_inbox_max")]
    max: usize,
    /// Long-poll window in seconds (cap 60; default 0 — return immediately).
    #[serde(default)]
    long_poll: u64,
}

fn default_inbox_max() -> usize {
    100
}

fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    s.parse::<AgentId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid agent id: {s}"))))
}

fn require_self_or_admin(auth: &AuthContext, agent_id: AgentId) -> Result<(), ApiError> {
    if auth.agent_id == agent_id || auth.scope.capabilities.has(Capability::Admin) {
        Ok(())
    } else {
        Err(ApiError::from(CoreError::forbidden(
            "token may only access its own agent inbox",
        )))
    }
}

async fn agent_inbox(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(agent_id_str): Path<String>,
    Query(q): Query<InboxQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let agent_id = parse_agent_id(&agent_id_str)?;
    require_self_or_admin(&auth, agent_id)?;

    let cursor = state
        .inbox
        .get_cursor(agent_id)
        .await
        .map_err(ApiError::from)?;
    let since = q.since.unwrap_or(cursor);
    let max = q.max.clamp(1, 1000);

    // First pass — return whatever is already there.
    let initial = state
        .store
        .load_since(since, max)
        .await
        .map_err(ApiError::from)?;
    if !initial.is_empty() {
        return Ok(Json(initial));
    }

    if q.long_poll == 0 {
        return Ok(Json(initial));
    }

    // Long-poll: wait for either a new event or the deadline (cap 60s).
    let wait = std::time::Duration::from_secs(q.long_poll.min(60));
    let mut rx = state.hub.subscribe();

    let parked = tokio::time::timeout(wait, async {
        loop {
            match rx.recv().await {
                Ok(env) if env.seq > since => return Some(env),
                Ok(_) => continue, // an older event slipped through — ignore
                // Treat both `Lagged` and `Closed` as "give up waiting" —
                // the client will re-poll and pick up history from the
                // store on the next call.
                Err(_) => return None,
            }
        }
    })
    .await
    .ok()
    .flatten();

    let out: Vec<_> = parked.into_iter().collect();
    Ok(Json(out))
}

#[derive(Deserialize)]
struct AckBody {
    up_to_seq: u64,
}

#[derive(serde::Serialize)]
struct AckResponse {
    last_acked_seq: u64,
}

async fn agent_inbox_ack(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(agent_id_str): Path<String>,
    Json(body): Json<AckBody>,
) -> Result<impl IntoResponse, ApiError> {
    let agent_id = parse_agent_id(&agent_id_str)?;
    require_self_or_admin(&auth, agent_id)?;
    let new_cursor = state
        .inbox
        .ack(agent_id, body.up_to_seq)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(AckResponse {
        last_acked_seq: new_cursor,
    }))
}

// ── Webhook admin handlers ────────────────────────────────────────────────────

async fn create_webhook(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<NewWebhook>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::WebhookWrite)
        .map_err(ApiError::from_missing_cap)?;
    if body.url.trim().is_empty() {
        return Err(ApiError::from(CoreError::validation(
            "webhook url must not be empty",
        )));
    }
    if body.secret.trim().is_empty() {
        return Err(ApiError::from(CoreError::validation(
            "webhook secret must not be empty",
        )));
    }
    let webhook = body.into_webhook();
    state
        .webhooks
        .insert(webhook.clone())
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(webhook)))
}

async fn list_webhooks(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::WebhookRead)
        .map_err(ApiError::from_missing_cap)?;
    let list = state.webhooks.list_all().await.map_err(ApiError::from)?;
    Ok(Json(list))
}

async fn patch_webhook(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(patch): Json<WebhookPatch>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::WebhookWrite)
        .map_err(ApiError::from_missing_cap)?;
    let id = id_str.parse::<WebhookId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid webhook id: {id_str}"
        )))
    })?;
    let updated = state
        .webhooks
        .patch(id, patch)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("webhook {id_str}"))))?;
    Ok(Json(updated))
}

async fn delete_webhook(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::WebhookWrite)
        .map_err(ApiError::from_missing_cap)?;
    let id = id_str.parse::<WebhookId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid webhook id: {id_str}"
        )))
    })?;
    let removed = state.webhooks.delete(id).await.map_err(ApiError::from)?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::from(CoreError::not_found(format!(
            "webhook {id_str}"
        ))))
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Derive the [`Actor`] to attribute to a dispatched command.
///
/// - `actor_strict` feature **enabled**: always use the token-derived actor
///   (Bot → `Agent`, Pat/Svc → `User`), ignoring any client-supplied override.
/// - `actor_strict` feature **disabled** (default): prefer `client_actor` when
///   `Some`, fall back to the token-derived actor.  Preserves legacy behaviour
///   for callers that do not yet supply an explicit actor.
fn actor_from(auth: &AuthContext, client_actor: Option<Actor>) -> Actor {
    #[cfg(feature = "actor_strict")]
    {
        let _ = client_actor;
        auth.actor()
    }
    #[cfg(not(feature = "actor_strict"))]
    {
        client_actor.unwrap_or_else(|| auth.actor())
    }
}

fn capability_for_command(cmd: &Command) -> Capability {
    match cmd {
        Command::CreateTask { .. }
        | Command::UpdateTask { .. }
        | Command::CompleteTask { .. }
        | Command::DeleteTask { .. }
        | Command::SetStatus { .. }
        | Command::SetPriority { .. }
        | Command::SplitTask { .. }
        | Command::BulkSetStatus { .. } => Capability::TaskWrite,
        Command::CreateProject { .. }
        | Command::UpdateProjectSettings { .. }
        | Command::UpdateProject { .. }
        | Command::DeleteProject { .. }
        // Lifecycle rules are project/tenant-scoped configuration; evidence is
        // recorded against the same scopes.
        | Command::CreateRule { .. }
        | Command::UpdateRule { .. }
        | Command::DisableRule { .. }
        | Command::RecordEvidence { .. } => Capability::ProjectWrite,
        Command::RecordAgentAction { .. } => Capability::AgentDispatch,
        Command::AddComment { .. }
        | Command::EditComment { .. }
        | Command::DeleteComment { .. } => Capability::CommentWrite,
        // Plan commands
        Command::CreatePlan { .. }
        | Command::UpdatePlan { .. }
        | Command::ArchivePlan { .. }
        | Command::AddPlanTask { .. }
        | Command::RemovePlanTask { .. }
        | Command::ReorderPlan { .. }
        | Command::SetPlanGoal { .. }
        | Command::SetPlanStatus { .. }
        | Command::BulkAttachToPlan { .. } => Capability::PlanWrite,
        // Run + signal + claim commands
        Command::StartRun { .. }
        | Command::RunStartStep { .. }
        | Command::RunFinishStep { .. }
        | Command::CompleteRun { .. }
        | Command::FailRun { .. }
        | Command::AbortRun { .. }
        | Command::AppendRunNote { .. }
        | Command::SendRunSignal { .. }
        | Command::RespondRunSignal { .. }
        | Command::AcquireClaim { .. }
        | Command::ReleaseClaim { .. }
        | Command::ReserveFiles { .. }
        | Command::ReleaseFiles { .. }
        | Command::CompleteWorkUnit { .. }
        | Command::ReleaseWorkUnit { .. }
        | Command::SetWorkUnitStatus { .. }
        // Handoff contracts are agent-plane coordination, same as the other
        // work-unit lifecycle commands.
        | Command::RequestHandoff { .. }
        | Command::AcceptHandoff { .. }
        | Command::RejectHandoff { .. } => Capability::RunWrite,
        Command::CreateWorkUnit { .. } => Capability::TaskWrite,
        // Agent session commands
        Command::StartAgentSession { .. }
        | Command::EndAgentSession { .. }
        | Command::UpdateAgentSessionPlan { .. }
        | Command::AttachSessionArtifact { .. } => Capability::AgentDispatch,
        // Relation commands (§3.2)
        Command::LinkTasks { .. } | Command::UnlinkTasks { .. } => Capability::TaskRelationWrite,
        // Document commands (PR1)
        Command::CreateDocument { .. }
        | Command::ReplaceDocumentContent { .. }
        | Command::AppendDocumentContent { .. }
        | Command::RenameDocument { .. }
        | Command::ArchiveDocument { .. }
        | Command::SetDocumentStatus { .. }
        | Command::LinkDocumentToTask { .. } => Capability::DocumentWrite,
    }
}

// ── Plan handlers (W3.1) ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreatePlanBody {
    plan: NewPlan,
    #[serde(default)]
    external_ref: Option<(String, String, String)>,
}

async fn create_plan(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<CreatePlanBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanWrite)
        .map_err(ApiError::from_missing_cap)?;
    let envs = state
        .commands
        .dispatch(
            Command::CreatePlan {
                plan: body.plan,
                external_ref: body.external_ref,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let plan_id = envs
        .iter()
        .find_map(|e| match &e.payload {
            Event::PlanCreated { plan } => Some(plan.id),
            _ => None,
        })
        .ok_or_else(|| ApiError::from(CoreError::storage("expected PlanCreated event")))?;
    let last = envs.last();
    Ok((
        StatusCode::CREATED,
        Json(MutationResponse {
            success: true,
            event_id: last.map(|e| e.id),
            event_seq: last.map(|e| e.seq),
            data: serde_json::json!({ "plan_id": plan_id }),
            warnings: vec![],
            client_command_id: None,
        }),
    ))
}

#[derive(Deserialize)]
struct UpdatePlanBody {
    patch: PlanPatch,
}

async fn update_plan(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<UpdatePlanBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanWrite)
        .map_err(ApiError::from_missing_cap)?;
    let plan_id = id_str
        .parse::<PlanId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid plan id: {id_str}"))))?;
    let envs = state
        .commands
        .dispatch(
            Command::UpdatePlan {
                id: plan_id,
                patch: body.patch,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "plan_id": plan_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

async fn get_plan(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanRead)
        .map_err(ApiError::from_missing_cap)?;
    let plan_id = parse_plan_ref(&id_str)?;
    let plan = state
        .plans
        .get(plan_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("plan {id_str}"))))?;
    let progress = state
        .plans
        .get_progress(plan_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(
        serde_json::json!({ "plan": plan, "progress": progress, "slug": plan_url_slug(&plan) }),
    ))
}

fn parse_plan_ref(raw: &str) -> Result<PlanId, ApiError> {
    if let Ok(id) = raw.parse::<PlanId>() {
        return Ok(id);
    }

    if let Some((_, id_part)) = raw.rsplit_once("-pln_") {
        return format!("pln_{id_part}")
            .parse::<PlanId>()
            .map_err(|_| ApiError::from(CoreError::validation(format!("invalid plan id: {raw}"))));
    }

    if raw.len() > 36 {
        let (slug_part, id_part) = raw.split_at(raw.len() - 36);
        if slug_part.ends_with('-') {
            if let Ok(id) = id_part.parse::<PlanId>() {
                return Ok(id);
            }
        }
    }

    Err(ApiError::from(CoreError::validation(format!(
        "invalid plan id: {raw}"
    ))))
}

fn plan_url_slug(plan: &Plan) -> String {
    format!("{}-{}", slugify_title(&plan.title), plan.id.as_uuid())
}

async fn get_plan_progress(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanRead)
        .map_err(ApiError::from_missing_cap)?;
    let plan_id = id_str
        .parse::<PlanId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid plan id: {id_str}"))))?;

    let mut summary = state
        .plans
        .get_progress_summary(plan_id)
        .await
        .map_err(ApiError::from)?;

    let resolver = NextTaskResolver {
        plans: state.plans.as_ref() as &dyn PlanRepository,
        tasks: state.tasks.as_ref(),
        claims: state.claims.as_ref(),
        relations: Some(state.relations.as_ref()),
    };
    if let Some(next) = resolver
        .next(plan_id, RunId::new(), auth.agent_id, None)
        .await
        .map_err(ApiError::from)?
    {
        summary.next_ready = Some(next.task_id);
    }

    Ok(Json(summary))
}

async fn get_plan_graph(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanRead)
        .map_err(ApiError::from_missing_cap)?;
    let plan_id = id_str
        .parse::<PlanId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid plan id: {id_str}"))))?;
    let graph = plan_readiness::plan_graph(&state.plans, &state.tasks, &state.relations, plan_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(graph))
}

async fn get_plan_fanout(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanRead)
        .map_err(ApiError::from_missing_cap)?;
    let plan_id = id_str
        .parse::<PlanId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid plan id: {id_str}"))))?;
    let waves = plan_readiness::plan_fanout(&state.plans, &state.tasks, &state.relations, plan_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(waves))
}

async fn get_can_start(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;
    let task_id = id_str
        .parse::<TaskId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid task id: {id_str}"))))?;
    let readiness = plan_readiness::can_start(&state.tasks, &state.relations, task_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(readiness))
}

/// `GET /v1/tasks/{id}/plans` — every plan that contains this task.
///
/// Backed by `idx_plan_tasks_task` (migration 0008), so the join is
/// bounded even with 10k+ tasks. Returns plans verbatim; callers that
/// only need title/status pick those fields off the response.
async fn list_task_plans(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanRead)
        .map_err(ApiError::from_missing_cap)?;
    let task_id = id_str
        .parse::<TaskId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid task id: {id_str}"))))?;
    let plans = state
        .plans
        .list_plans_for_task(task_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(plans))
}

#[derive(Deserialize, Default)]
struct ListPlansQuery {
    project_id: Option<String>,
    /// **Required.** Single status, comma-separated list, or `all`.
    status: Option<String>,
    /// Max rows to return. Defaults to 10, capped at 100.
    limit: Option<usize>,
}

/// Parse the required plan `status` query parameter.
///
/// `all` → `Ok(None)` (no status predicate). Otherwise returns the parsed
/// statuses for an `IN (...)` filter.
fn parse_plan_status_filter(raw: Option<&str>) -> Result<Option<Vec<PlanStatus>>, ApiError> {
    let Some(raw) = raw else {
        return Err(ApiError::from(CoreError::validation(
            "status is required (e.g. status=active, status=draft,active, or status=all)",
        )));
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ApiError::from(CoreError::validation(
            "status is required (e.g. status=active, status=draft,active, or status=all)",
        )));
    }
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok(None);
    }

    let mut out: Vec<PlanStatus> = Vec::new();
    for token in trimmed.split(',') {
        let t = token.trim();
        if t.is_empty() {
            continue;
        }
        let parsed = match t {
            "draft" => PlanStatus::Draft,
            "active" => PlanStatus::Active,
            "completed" => PlanStatus::Completed,
            "abandoned" => PlanStatus::Abandoned,
            other => {
                return Err(ApiError::from(CoreError::validation(format!(
                    "unknown plan status: {other}"
                ))))
            }
        };
        if !out.contains(&parsed) {
            out.push(parsed);
        }
    }
    if out.is_empty() {
        return Err(ApiError::from(CoreError::validation(
            "status filter is empty after trimming",
        )));
    }
    Ok(Some(out))
}

async fn list_plans(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<ListPlansQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanRead)
        .map_err(ApiError::from_missing_cap)?;

    let status_filter = parse_plan_status_filter(q.status.as_deref())?;

    let limit = bounded_collection_limit(q.limit);
    let mut plans = match q.project_id.as_deref() {
        Some(pid) => {
            let project_id = pid.parse::<ProjectId>().map_err(|_| {
                ApiError::from(CoreError::validation(format!("invalid project id: {pid}")))
            })?;
            state
                .plans
                .list_by_project(project_id, status_filter.as_deref())
                .await
                .map_err(ApiError::from)?
        }
        None => {
            return Err(ApiError::from(CoreError::validation(
                "project_id is required",
            )))
        }
    };
    plans.truncate(limit);
    Ok(Json(plans))
}

#[derive(Deserialize)]
struct AddPlanTaskBody {
    task_id: TaskId,
    #[serde(default)]
    position: Option<u32>,
    #[serde(default)]
    depends_on: Option<Vec<TaskId>>,
}

async fn add_plan_task(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<AddPlanTaskBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanWrite)
        .map_err(ApiError::from_missing_cap)?;
    let plan_id = id_str
        .parse::<PlanId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid plan id: {id_str}"))))?;
    let envs = state
        .commands
        .dispatch(
            Command::AddPlanTask {
                plan_id,
                task_id: body.task_id,
                position: body.position,
                depends_on: body.depends_on,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "plan_id": plan_id, "task_id": body.task_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

async fn remove_plan_task(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path((id_str, task_id_str)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanWrite)
        .map_err(ApiError::from_missing_cap)?;
    let plan_id = id_str
        .parse::<PlanId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid plan id: {id_str}"))))?;
    let task_id = task_id_str.parse::<TaskId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid task id: {task_id_str}"
        )))
    })?;
    let envs = state
        .commands
        .dispatch(
            Command::RemovePlanTask { plan_id, task_id },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "plan_id": plan_id, "task_id": task_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

#[derive(Deserialize)]
struct ReorderPlanBody {
    order: Vec<TaskId>,
}

async fn reorder_plan(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<ReorderPlanBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanWrite)
        .map_err(ApiError::from_missing_cap)?;
    let plan_id = id_str
        .parse::<PlanId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid plan id: {id_str}"))))?;
    let envs = state
        .commands
        .dispatch(
            Command::ReorderPlan {
                plan_id,
                order: body.order,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "plan_id": plan_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

async fn archive_plan(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanWrite)
        .map_err(ApiError::from_missing_cap)?;
    let plan_id = id_str
        .parse::<PlanId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid plan id: {id_str}"))))?;
    let envs = state
        .commands
        .dispatch(
            Command::ArchivePlan { id: plan_id },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "plan_id": plan_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

#[derive(Deserialize)]
struct SetPlanStatusBody {
    status: PlanStatus,
}

/// `POST /v1/plans/{id}/status` — transition a plan into a different
/// lifecycle state (Draft / Active / Completed / Abandoned). Emits
/// `PlanStatusChanged`. Quick-fix surface for §3.5 — separate from the
/// metadata-only `PATCH /v1/plans/{id}` so status changes get their own
/// event semantics.
async fn set_plan_status(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<SetPlanStatusBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanWrite)
        .map_err(ApiError::from_missing_cap)?;
    let plan_id = id_str
        .parse::<PlanId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid plan id: {id_str}"))))?;
    let envs = state
        .commands
        .dispatch(
            Command::SetPlanStatus {
                plan_id,
                status: body.status,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "plan_id": plan_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

#[derive(Deserialize, Default)]
struct NextTaskQuery {
    run_id: Option<String>,
    claim_ttl_secs: Option<u64>,
}

#[derive(Deserialize, Default)]
struct DrainNextBody {
    run_id: Option<RunId>,
    claim_ttl_secs: Option<u32>,
}

async fn get_next_task(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Query(q): Query<NextTaskQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanRead)
        .map_err(ApiError::from_missing_cap)?;
    let plan_id = id_str
        .parse::<PlanId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid plan id: {id_str}"))))?;
    let run_id = q
        .run_id
        .as_deref()
        .map(|s| {
            s.parse::<RunId>()
                .map_err(|_| ApiError::from(CoreError::validation(format!("invalid run id: {s}"))))
        })
        .transpose()?
        .unwrap_or_else(RunId::new);
    let claim_ttl = q.claim_ttl_secs.map(std::time::Duration::from_secs);

    let resolver = NextTaskResolver {
        plans: state.plans.as_ref() as &dyn PlanRepository,
        tasks: state.tasks.as_ref(),
        claims: state.claims.as_ref(),
        relations: Some(state.relations.as_ref()),
    };
    let result = resolver
        .next(plan_id, run_id, auth.agent_id, claim_ttl)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(result.map(|n| {
        serde_json::json!({
            "task_id": n.task_id,
            "position": n.position,
            "claim_expires_at": n.claim_expires_at,
        })
    })))
}

/// Atomically resolve + claim the next ready task in one plan, coupling the
/// claim with an `in_progress` transition. Returns the task JSON on success, or
/// `None` when the plan has no unclaimed ready task. Shared by single-plan
/// `drain_next` and the project-wide ready drain.
async fn drain_one_plan(
    state: &AppState,
    auth: &AuthContext,
    plan_id: PlanId,
    run_id: RunId,
    ttl_secs: u32,
) -> Result<Option<serde_json::Value>, ApiError> {
    let ttl = std::time::Duration::from_secs(ttl_secs as u64);

    // Bound retries by the plan's task count: each Busy means a competing agent
    // just claimed that candidate, and the claim-aware resolver will skip it on
    // the next pass — so at most one full sweep of the plan is ever needed.
    let max_attempts = state
        .plans
        .list_tasks_ordered(plan_id)
        .await
        .map_err(ApiError::from)?
        .len()
        .max(1);

    let resolver = NextTaskResolver {
        plans: state.plans.as_ref() as &dyn PlanRepository,
        tasks: state.tasks.as_ref(),
        claims: state.claims.as_ref(),
        relations: Some(state.relations.as_ref()),
    };

    for _ in 0..max_attempts {
        let Some(next) = resolver
            .next(plan_id, run_id, auth.agent_id, Some(ttl))
            .await
            .map_err(ApiError::from)?
        else {
            return Ok(None);
        };

        // Atomic compare-and-set: if a competitor grabbed it between resolve and
        // here, retry; the resolver excludes their claim on the next iteration.
        let expires_at = match state
            .claims
            .try_acquire(
                auth.agent_id,
                next.task_id,
                chrono::Duration::seconds(ttl_secs as i64),
            )
            .await
            .map_err(ApiError::from)?
        {
            ClaimOutcome::Busy { .. } => continue,
            ClaimOutcome::Acquired { expires_at } => expires_at,
        };

        // Emit AgentClaimed for audit + WebSocket sync (idempotent upsert).
        let envs = state
            .commands
            .dispatch(
                Command::AcquireClaim {
                    agent_id: auth.agent_id,
                    task_id: next.task_id,
                    ttl_secs,
                },
                actor_from(auth, None),
            )
            .await
            .map_err(ApiError::from)?;
        let last = envs.last();

        // Couple the claim with a status transition (beads-style): move a ready
        // task into `in_progress` so the resolver's status filter and dashboards
        // reflect that it is being worked. The claim holder is the de-facto
        // assignee (recorded in `agent_claims`).
        if let Some(task) = state
            .tasks
            .get(next.task_id)
            .await
            .map_err(ApiError::from)?
        {
            if task.status != daruma_domain::Status::InProgress && !task.status.is_terminal() {
                let status_result = state
                    .commands
                    .dispatch(
                        Command::SetStatus {
                            id: next.task_id,
                            status: daruma_domain::Status::InProgress,
                            force: true,
                        },
                        actor_from(auth, None),
                    )
                    .await;
                if let Err(err) = status_result {
                    // Gate compensation (docs/LIFECYCLE_RULES_SPEC.md §3,
                    // invariant 7): the claim was acquired BEFORE the gated
                    // transition; release it so a rule-blocked task does not
                    // stay claimed with no work happening. Best-effort — the
                    // claim TTL remains the fallback.
                    let _ = state
                        .commands
                        .dispatch(
                            Command::ReleaseClaim {
                                agent_id: auth.agent_id,
                                task_id: next.task_id,
                            },
                            actor_from(auth, None),
                        )
                        .await;
                    return Err(ApiError::from(err));
                }
            }
        }

        return Ok(Some(serde_json::json!({
            "task_id": next.task_id,
            "plan_id": plan_id,
            "position": next.position,
            "claim_expires_at": expires_at,
            "claim": {
                "agent_id": auth.agent_id,
                "event_id": last.map(|e| e.id),
                "event_seq": last.map(|e| e.seq),
            }
        })));
    }

    // Exhausted: every ready candidate was taken by a competing agent.
    Ok(None)
}

async fn drain_next_task(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<DrainNextBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanRead)
        .map_err(ApiError::from_missing_cap)?;
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let plan_id = id_str
        .parse::<PlanId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid plan id: {id_str}"))))?;
    let run_id = body.run_id.unwrap_or_else(RunId::new);
    let ttl_secs = body.claim_ttl_secs.unwrap_or(300);

    let task = drain_one_plan(&state, &auth, plan_id, run_id, ttl_secs).await?;
    Ok(Json(task.unwrap_or(serde_json::Value::Null)))
}

#[derive(Deserialize)]
struct ProjectReadyQuery {
    project_id: String,
}

/// `GET /v1/ready?project_id=` — the project-wide ready pool: tasks across all
/// active plans whose dependencies are satisfied and that no other agent holds.
async fn project_ready(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<ProjectReadyQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanRead)
        .map_err(ApiError::from_missing_cap)?;
    let project_id = q.project_id.parse::<ProjectId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid project id: {}",
            q.project_id
        )))
    })?;

    let plans = state
        .plans
        .list_by_project(project_id, Some(&[PlanStatus::Active]))
        .await
        .map_err(ApiError::from)?;

    let mut ready = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for plan in &plans {
        let waves =
            plan_readiness::plan_fanout(&state.plans, &state.tasks, &state.relations, plan.id)
                .await
                .map_err(ApiError::from)?;
        if let Some(wave0) = waves.first() {
            for task_id in &wave0.tasks {
                if seen.insert(*task_id)
                    && state
                        .claims
                        .is_claimed_by_other(*task_id, auth.agent_id)
                        .await
                        .map_err(ApiError::from)?
                        .is_none()
                {
                    ready.push(serde_json::json!({ "task_id": task_id, "plan_id": plan.id }));
                }
            }
        }
    }

    Ok(Json(serde_json::json!({ "ready": ready })))
}

/// `POST /v1/ready/drain?project_id=` — atomically claim the next ready task
/// across the project's active plans. N agents calling this concurrently each
/// get a distinct task.
async fn project_ready_drain(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<ProjectReadyQuery>,
    Json(body): Json<DrainNextBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::PlanRead)
        .map_err(ApiError::from_missing_cap)?;
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let project_id = q.project_id.parse::<ProjectId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid project id: {}",
            q.project_id
        )))
    })?;
    let run_id = body.run_id.unwrap_or_else(RunId::new);
    let ttl_secs = body.claim_ttl_secs.unwrap_or(300);

    let plans = state
        .plans
        .list_by_project(project_id, Some(&[PlanStatus::Active]))
        .await
        .map_err(ApiError::from)?;

    for plan in &plans {
        if let Some(task) = drain_one_plan(&state, &auth, plan.id, run_id, ttl_secs).await? {
            return Ok(Json(task));
        }
    }
    Ok(Json(serde_json::Value::Null))
}

/// `GET /v1/doctor?project_id=` — reconcile parallel-agent state. Reports tasks
/// stuck `in_progress` with **no live claim** (an agent likely crashed mid-task,
/// leaving its TTL claim to lapse): they are reclaimable but invisible to the
/// resolver's status filter until reopened. Git-history cross-ref
/// (committed-but-open) is intentionally an orchestrator-side concern — the
/// tracker stays VCS-agnostic (see ADR parallel-agent-isolation).
async fn project_doctor(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<ProjectReadyQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunRead)
        .map_err(ApiError::from_missing_cap)?;
    let project_id = q.project_id.parse::<ProjectId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid project id: {}",
            q.project_id
        )))
    })?;

    let in_progress = state
        .tasks
        .list_by_project_filtered(Some(project_id), &[daruma_domain::Status::InProgress])
        .await
        .map_err(ApiError::from)?;

    let mut stale = Vec::new();
    for task in &in_progress {
        if state
            .claims
            .is_claimed(task.id)
            .await
            .map_err(ApiError::from)?
            .is_none()
        {
            stale.push(serde_json::json!({
                "task_id": task.id,
                "title": task.title,
                "reason": "in_progress with no live claim (likely abandoned; reclaimable)",
            }));
        }
    }

    Ok(Json(serde_json::json!({
        "in_progress_total": in_progress.len(),
        "stale_in_progress": stale,
    })))
}

#[derive(Deserialize)]
struct SuggestFilesQuery {
    task_id: String,
}

/// `GET /v1/leases/suggest?task_id=` — suggest path globs to reserve for a task
/// by extracting path-like tokens from its title + description. A lightweight
/// heuristic (no code index): tokens containing `/` or a known source extension.
async fn suggest_files(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<SuggestFilesQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;
    let task_id = q.task_id.parse::<TaskId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid task id: {}",
            q.task_id
        )))
    })?;
    let task = state
        .tasks
        .get(task_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("task {task_id}"))))?;

    let text = format!("{} {}", task.title, task.description);
    let paths = suggest_paths_from_text(&text);
    Ok(Json(
        serde_json::json!({ "task_id": task_id, "suggested_paths": paths }),
    ))
}

/// Extract path-like tokens (contain `/` or end in a common source extension).
fn suggest_paths_from_text(text: &str) -> Vec<String> {
    const EXTS: &[&str] = &[
        ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".sql", ".toml", ".md", ".css", ".html",
    ];
    let mut out = Vec::new();
    for raw in text
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '`' | '"' | '\''))
    {
        let tok = raw.trim_matches(|c: char| matches!(c, '.' | ':' | '#' | '*'));
        if tok.is_empty() || tok.len() > 200 {
            continue;
        }
        let looks_pathy = tok.contains('/') || EXTS.iter().any(|e| tok.ends_with(e));
        if looks_pathy && !out.contains(&tok.to_string()) {
            out.push(tok.to_string());
        }
    }
    out
}

// ── Run handlers (W3.1) ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct StartRunBody {
    plan_id: PlanId,
    agent_id: AgentId,
    #[serde(default)]
    parent_run_id: Option<RunId>,
}

async fn start_run(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<StartRunBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let envs = state
        .commands
        .dispatch(
            Command::StartRun {
                plan_id: body.plan_id,
                agent_id: body.agent_id,
                parent_run_id: body.parent_run_id,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let run_id = envs
        .iter()
        .find_map(|e| match &e.payload {
            Event::RunStarted { run } => Some(run.id),
            _ => None,
        })
        .ok_or_else(|| ApiError::from(CoreError::storage("expected RunStarted event")))?;
    let last = envs.last();
    Ok((
        StatusCode::CREATED,
        Json(MutationResponse {
            success: true,
            event_id: last.map(|e| e.id),
            event_seq: last.map(|e| e.seq),
            data: serde_json::json!({ "run_id": run_id }),
            warnings: vec![],
            client_command_id: None,
        }),
    ))
}

#[derive(Deserialize)]
struct RunStartStepBody {
    task_id: TaskId,
}

async fn run_start_step(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<RunStartStepBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let run_id = id_str
        .parse::<RunId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid run id: {id_str}"))))?;
    let envs = state
        .commands
        .dispatch(
            Command::RunStartStep {
                run_id,
                task_id: body.task_id,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "run_id": run_id, "task_id": body.task_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

#[derive(Deserialize)]
struct RunFinishStepBody {
    task_id: TaskId,
    outcome: RunOutcome,
}

async fn run_finish_step(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<RunFinishStepBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let run_id = id_str
        .parse::<RunId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid run id: {id_str}"))))?;
    let envs = state
        .commands
        .dispatch(
            Command::RunFinishStep {
                run_id,
                task_id: body.task_id,
                outcome: body.outcome,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "run_id": run_id, "task_id": body.task_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

async fn complete_run(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let run_id = id_str
        .parse::<RunId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid run id: {id_str}"))))?;
    let envs = state
        .commands
        .dispatch(Command::CompleteRun { run_id }, actor_from(&auth, None))
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "run_id": run_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

#[derive(Deserialize)]
struct AbortRunBody {
    reason: String,
}

async fn abort_run(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<AbortRunBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let run_id = id_str
        .parse::<RunId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid run id: {id_str}"))))?;
    let envs = state
        .commands
        .dispatch(
            Command::AbortRun {
                run_id,
                reason: body.reason,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "run_id": run_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

#[derive(Deserialize)]
struct SendSignalBody {
    kind: SignalKind,
}

async fn send_run_signal(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<SendSignalBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let run_id = id_str
        .parse::<RunId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid run id: {id_str}"))))?;
    let envs = state
        .commands
        .dispatch(
            Command::SendRunSignal {
                run_id,
                kind: body.kind,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "run_id": run_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

#[derive(Deserialize)]
struct AppendRunNoteBody {
    body: String,
}

async fn append_run_note(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<AppendRunNoteBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let run_id = id_str
        .parse::<RunId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid run id: {id_str}"))))?;
    let envs = state
        .commands
        .dispatch(
            Command::AppendRunNote {
                run_id,
                body: body.body,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;

    // Project the emitted event into the response so MCP / clients can
    // immediately return the canonical note id + timestamp.
    let note = envs
        .into_iter()
        .find_map(|e| {
            if let Event::RunNoteAppended {
                run_id,
                note_id,
                body,
                by_actor,
                at,
            } = e.payload
            {
                Some(daruma_domain::RunNote {
                    id: note_id,
                    run_id,
                    body,
                    author: by_actor,
                    created_at: at,
                })
            } else {
                None
            }
        })
        .ok_or_else(|| ApiError::from(CoreError::storage("expected RunNoteAppended event")))?;

    Ok((StatusCode::CREATED, Json(note)))
}

#[derive(Deserialize, Default)]
struct ListRunNotesQuery {
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    after: Option<String>,
}

async fn list_run_notes(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Query(q): Query<ListRunNotesQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunRead)
        .map_err(ApiError::from_missing_cap)?;
    let run_id = id_str
        .parse::<RunId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid run id: {id_str}"))))?;
    let limit = q.limit.unwrap_or(50);
    let after = q
        .after
        .as_deref()
        .map(|s| {
            s.parse::<daruma_shared::RunNoteId>().map_err(|_| {
                ApiError::from(CoreError::validation(format!("invalid after cursor: {s}")))
            })
        })
        .transpose()?;

    let notes = state
        .run_notes
        .list_for_run(run_id, limit, after)
        .await
        .map_err(ApiError::from)?;

    let next_cursor = notes.last().map(|n| n.id.to_string());
    Ok(Json(json!({
        "notes": notes,
        "next_cursor": next_cursor,
    })))
}

#[derive(Deserialize)]
struct RespondSignalBody {
    choice: String,
}

async fn respond_run_signal(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<RespondSignalBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let run_id = id_str
        .parse::<RunId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid run id: {id_str}"))))?;
    let envs = state
        .commands
        .dispatch(
            Command::RespondRunSignal {
                run_id,
                choice: body.choice,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "run_id": run_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

// ── Session handlers (W3.1) ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct StartSessionBody {
    agent_id: AgentId,
    #[serde(default)]
    parent_agent_id: Option<AgentId>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

async fn start_session(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<StartSessionBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::AgentDispatch)
        .map_err(ApiError::from_missing_cap)?;
    let envs = state
        .commands
        .dispatch(
            Command::StartAgentSession {
                agent_id: body.agent_id,
                parent_agent_id: body.parent_agent_id,
                metadata: body.metadata,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let session_id = envs
        .iter()
        .find_map(|e| match &e.payload {
            Event::AgentSessionStarted { session } => Some(session.id),
            _ => None,
        })
        .ok_or_else(|| ApiError::from(CoreError::storage("expected AgentSessionStarted event")))?;
    let session = state
        .sessions
        .get(session_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::storage("session missing after start")))?;
    let last = envs.last();
    Ok((
        StatusCode::CREATED,
        Json(MutationResponse {
            success: true,
            event_id: last.map(|e| e.id),
            event_seq: last.map(|e| e.seq),
            data: serde_json::to_value(&session).unwrap_or(serde_json::Value::Null),
            warnings: vec![],
            client_command_id: None,
        }),
    ))
}

#[derive(Deserialize)]
struct ListSessionsQuery {
    agent_id: String,
}

async fn list_sessions(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<ListSessionsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::AgentDispatch)
        .map_err(ApiError::from_missing_cap)?;
    let agent_id = q.agent_id.parse::<AgentId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid agent_id: {}",
            q.agent_id
        )))
    })?;
    let sessions = state
        .sessions
        .list_for_agent(agent_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "sessions": sessions })))
}

async fn get_session(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::AgentDispatch)
        .map_err(ApiError::from_missing_cap)?;
    let session_id = id_str.parse::<AgentSessionId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid session id: {id_str}"
        )))
    })?;
    let session = state
        .sessions
        .get(session_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("session {session_id}"))))?;
    Ok(Json(session))
}

async fn end_session(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::AgentDispatch)
        .map_err(ApiError::from_missing_cap)?;
    let session_id = id_str.parse::<AgentSessionId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid session id: {id_str}"
        )))
    })?;
    let envs = state
        .commands
        .dispatch(
            Command::EndAgentSession { id: session_id },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "session_id": session_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

#[derive(Deserialize)]
struct UpdateSessionPlanBody {
    steps: Vec<AgentSessionPlanStep>,
}

async fn update_session_plan_steps(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<UpdateSessionPlanBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::AgentDispatch)
        .map_err(ApiError::from_missing_cap)?;
    let session_id = id_str.parse::<AgentSessionId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid session id: {id_str}"
        )))
    })?;
    let envs = state
        .commands
        .dispatch(
            Command::UpdateAgentSessionPlan {
                id: session_id,
                steps: body.steps,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "session_id": session_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

#[derive(Deserialize)]
struct AttachSessionArtifactBody {
    kind: SessionArtifactKind,
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

async fn attach_session_artifact(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<AttachSessionArtifactBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::AgentDispatch)
        .map_err(ApiError::from_missing_cap)?;
    let session_id = id_str.parse::<AgentSessionId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid session id: {id_str}"
        )))
    })?;
    let envs = state
        .commands
        .dispatch(
            Command::AttachSessionArtifact {
                session_id,
                kind: body.kind,
                reference: body.reference,
                metadata: body.metadata,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;

    let artifact = envs
        .iter()
        .find_map(|e| match &e.payload {
            Event::SessionArtifactAttached { artifact } => Some(artifact.clone()),
            _ => None,
        })
        .ok_or_else(|| {
            ApiError::from(CoreError::storage("expected SessionArtifactAttached event"))
        })?;

    Ok((StatusCode::CREATED, Json(artifact)))
}

async fn list_session_artifacts(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::AgentDispatch)
        .map_err(ApiError::from_missing_cap)?;
    let session_id = id_str.parse::<AgentSessionId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid session id: {id_str}"
        )))
    })?;
    let artifacts = state
        .sessions
        .list_artifacts(session_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({ "artifacts": artifacts })))
}

// ── Claim handlers (W3.1) ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AcquireClaimBody {
    agent_id: AgentId,
    task_id: TaskId,
    ttl_secs: u32,
}

async fn acquire_claim(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<AcquireClaimBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;

    // Atomic exclusive acquire: another agent's live claim blocks us.
    match state
        .claims
        .try_acquire(
            body.agent_id,
            body.task_id,
            chrono::Duration::seconds(body.ttl_secs as i64),
        )
        .await
        .map_err(ApiError::from)?
    {
        ClaimOutcome::Busy { holder, expires_at } => Ok(Json(MutationResponse {
            success: false,
            event_id: None,
            event_seq: None,
            data: serde_json::json!({
                "acquired": false,
                "task_id": body.task_id,
                "holder": holder,
                "claim_expires_at": expires_at,
                "reason": "task already claimed by another agent",
            }),
            warnings: vec![],
            client_command_id: None,
        })),
        ClaimOutcome::Acquired { expires_at } => {
            // Emit AgentClaimed for audit + WebSocket sync (idempotent upsert).
            let envs = state
                .commands
                .dispatch(
                    Command::AcquireClaim {
                        agent_id: body.agent_id,
                        task_id: body.task_id,
                        ttl_secs: body.ttl_secs,
                    },
                    actor_from(&auth, None),
                )
                .await
                .map_err(ApiError::from)?;
            let last = envs.last();
            Ok(Json(MutationResponse {
                success: true,
                event_id: last.map(|e| e.id),
                event_seq: last.map(|e| e.seq),
                data: serde_json::json!({
                    "acquired": true,
                    "agent_id": body.agent_id,
                    "task_id": body.task_id,
                    "claim_expires_at": expires_at,
                }),
                warnings: vec![],
                client_command_id: None,
            }))
        }
    }
}

async fn release_claim(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path((agent_id_str, task_id_str)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let agent_id = agent_id_str.parse::<AgentId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid agent id: {agent_id_str}"
        )))
    })?;
    let task_id = task_id_str.parse::<TaskId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid task id: {task_id_str}"
        )))
    })?;
    let envs = state
        .commands
        .dispatch(
            Command::ReleaseClaim { agent_id, task_id },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "agent_id": agent_id, "task_id": task_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

// ── Work-lease handlers (parallel-agent file coordination) ────────────────────

#[derive(Deserialize)]
struct ReserveFilesBody {
    agent_id: AgentId,
    task_id: TaskId,
    #[serde(default)]
    project_id: Option<ProjectId>,
    /// Repo-relative path globs (legacy field; becomes `file://` targets).
    #[serde(default)]
    paths: Vec<String>,
    /// Resource URIs (`file://`, `artifact://`, `contract://`, `env://`).
    /// Merged with `paths`; at least one of the two must be non-empty.
    #[serde(default)]
    targets: Vec<String>,
    /// Lease mode: `exclusive` (default) | `shared_read` | `review` | `intent`.
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    ttl_secs: Option<u32>,
}

/// `POST /v1/leases` — atomically reserve file/path globs for a task. Returns
/// `reserved: false` with the conflicting path + holder when another agent
/// already owns an overlapping area.
async fn reserve_files(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<ReserveFilesBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let mut targets = body.paths.clone();
    targets.extend(body.targets.iter().cloned());
    if targets.is_empty() {
        return Err(ApiError::from(CoreError::validation(
            "paths/targets must not be empty",
        )));
    }
    let mode = match body.mode.as_deref() {
        None => daruma_domain::LeaseMode::Exclusive,
        Some(raw) => daruma_domain::LeaseMode::parse(raw).ok_or_else(|| {
            ApiError::from(CoreError::validation(format!(
                "unknown lease mode `{raw}` — expected exclusive|shared_read|review|intent"
            )))
        })?,
    };
    let ttl_secs = body.ttl_secs.unwrap_or(300);

    match state
        .work_leases
        .try_reserve_targets(
            body.agent_id,
            body.task_id,
            body.project_id,
            targets,
            mode,
            chrono::Duration::seconds(ttl_secs as i64),
        )
        .await
        .map_err(ApiError::from)?
    {
        ReserveOutcome::Conflict {
            path,
            holder,
            holder_task,
        } => Ok(Json(MutationResponse {
            success: false,
            event_id: None,
            event_seq: None,
            data: serde_json::json!({
                "reserved": false,
                "task_id": body.task_id,
                "conflict_path": path,
                "holder": holder,
                "holder_task": holder_task,
                "reason": "path overlaps a lease held by another agent; negotiate via daruma_signal_send or take a different task",
            }),
            warnings: vec![],
            client_command_id: None,
        })),
        ReserveOutcome::Reserved { leases } => {
            // Project the reservation into the event log for audit + WS sync.
            let envs = state
                .commands
                .dispatch(
                    Command::ReserveFiles {
                        leases: leases.clone(),
                    },
                    actor_from(&auth, None),
                )
                .await
                .map_err(ApiError::from)?;
            let last = envs.last();
            Ok(Json(MutationResponse {
                success: true,
                event_id: last.map(|e| e.id),
                event_seq: last.map(|e| e.seq),
                data: serde_json::json!({ "reserved": true, "leases": leases }),
                warnings: vec![],
                client_command_id: None,
            }))
        }
    }
}

/// `DELETE /v1/leases/{agent_id}/{task_id}` — release all of an agent's leases
/// for a task (usually automatic on task completion).
async fn release_files(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path((agent_id_str, task_id_str)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunWrite)
        .map_err(ApiError::from_missing_cap)?;
    let agent_id = agent_id_str.parse::<AgentId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid agent id: {agent_id_str}"
        )))
    })?;
    let task_id = task_id_str.parse::<TaskId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid task id: {task_id_str}"
        )))
    })?;
    let envs = state
        .commands
        .dispatch(
            Command::ReleaseFiles { agent_id, task_id },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "agent_id": agent_id, "task_id": task_id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

#[derive(Deserialize)]
struct ActiveWorkQuery {
    project_id: Option<String>,
}

/// `GET /v1/leases` — the backlog of active work with affected files,
/// optionally scoped to a project.
async fn active_work(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<ActiveWorkQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::RunRead)
        .map_err(ApiError::from_missing_cap)?;
    let project_id = q
        .project_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<ProjectId>().map_err(|_| {
                ApiError::from(CoreError::validation(format!("invalid project id: {s}")))
            })
        })
        .transpose()?;
    let leases = state
        .work_leases
        .list_active(project_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({ "leases": leases })))
}

// ── Document handlers (PR1 §8) ────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateDocumentBody {
    new_doc: NewDocument,
}

/// `POST /v1/documents` — create a new document for a project.
async fn create_document(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<CreateDocumentBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::DocumentWrite)
        .map_err(ApiError::from_missing_cap)?;
    let envs = state
        .commands
        .dispatch(
            Command::CreateDocument {
                new_doc: body.new_doc,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;

    // The CreateDocument command emits exactly one DocumentCreated; surface
    // the new id so callers don't have to fish it out of the event payload.
    let document_id = envs
        .iter()
        .find_map(|env| match &env.payload {
            Event::DocumentCreated { document } => Some(document.id),
            _ => None,
        })
        .ok_or_else(|| ApiError::from(CoreError::storage("expected DocumentCreated event")))?;
    let last = envs.last();
    Ok((
        StatusCode::CREATED,
        Json(MutationResponse {
            success: true,
            event_id: last.map(|e| e.id),
            event_seq: last.map(|e| e.seq),
            data: serde_json::json!({ "document_id": document_id }),
            warnings: vec![],
            client_command_id: None,
        }),
    ))
}

/// `GET /v1/documents/{id}` — fetch a single document.
async fn get_document(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::DocumentRead)
        .map_err(ApiError::from_missing_cap)?;
    let id = id_str.parse::<DocumentId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid document id: {id_str}"
        )))
    })?;
    let doc = state
        .documents
        .get(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::from(CoreError::not_found(format!("document {id_str}"))))?;

    // Passive read-tracking (Audit primitives task A): record the read,
    // throttled per (document, actor) to ≤ once/hour so repeated fetches don't
    // churn the row. Best-effort — a tracking write must never fail the read.
    let reader = read_actor_label(&actor_from(&auth, None));
    if let Err(e) = state
        .documents
        .mark_read(id, &reader, daruma_shared::time::now(), READ_THROTTLE)
        .await
    {
        tracing::warn!(error = %e, document_id = %id, "doc read-tracking update failed");
    }

    Ok(Json(serde_json::json!({ "document": doc })))
}

/// Throttle window for document read-tracking: a read by the same actor within
/// this window of the last is not re-counted (keeps the projection write rate low).
const READ_THROTTLE: std::time::Duration = std::time::Duration::from_secs(3600);

/// Stable label for "who read this document": the agent's display name when
/// present, else the actor kind (`user` / `agent`). Mirrors the `ActorRef`
/// triple the rest of the system stores.
fn read_actor_label(actor: &Actor) -> String {
    let aref = daruma_domain::ActorRef::from_actor(actor);
    aref.name.unwrap_or(aref.kind)
}

#[derive(Deserialize)]
struct PatchDocumentBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content: Option<String>,
    /// Lifecycle status (OSS task 019eb65b).
    #[serde(default)]
    status: Option<daruma_domain::DocumentStatus>,
    /// Task binding. Present-vs-absent matters: absent = leave the link
    /// alone, explicit `null` = unlink back to a project-level document.
    #[serde(default, deserialize_with = "deserialize_present")]
    task_id: Option<Option<TaskId>>,
}

/// Deserialize a field so an *absent* key stays `None` (via
/// `#[serde(default)]`) while a present key — including explicit `null` —
/// becomes `Some(inner)`. The standard double-`Option` PATCH trick.
fn deserialize_present<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

/// `PATCH /v1/documents/{id}` — rename, replace the body, change lifecycle
/// status, and/or re-bind to a task. The commands dispatch sequentially;
/// whichever is requested via the body is emitted (several can be requested
/// in a single call). The body must specify at least one field.
async fn patch_document(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<PatchDocumentBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::DocumentWrite)
        .map_err(ApiError::from_missing_cap)?;
    let id = id_str.parse::<DocumentId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid document id: {id_str}"
        )))
    })?;
    if body.title.is_none()
        && body.content.is_none()
        && body.status.is_none()
        && body.task_id.is_none()
    {
        return Err(ApiError::from(CoreError::validation(
            "patch must set at least one of `title`, `content`, `status`, `task_id`",
        )));
    }

    let mut last_env: Option<daruma_events::EventEnvelope> = None;

    if let Some(title) = body.title {
        let envs = state
            .commands
            .dispatch(
                Command::RenameDocument {
                    document_id: id,
                    title,
                },
                actor_from(&auth, None),
            )
            .await
            .map_err(ApiError::from)?;
        last_env = envs.into_iter().last().or(last_env);
    }
    if let Some(content) = body.content {
        let envs = state
            .commands
            .dispatch(
                Command::ReplaceDocumentContent {
                    document_id: id,
                    content,
                },
                actor_from(&auth, None),
            )
            .await
            .map_err(ApiError::from)?;
        last_env = envs.into_iter().last().or(last_env);
    }
    if let Some(status) = body.status {
        let envs = state
            .commands
            .dispatch(
                Command::SetDocumentStatus {
                    document_id: id,
                    status,
                },
                actor_from(&auth, None),
            )
            .await
            .map_err(ApiError::from)?;
        last_env = envs.into_iter().last().or(last_env);
    }
    if let Some(task_id) = body.task_id {
        let envs = state
            .commands
            .dispatch(
                Command::LinkDocumentToTask {
                    document_id: id,
                    task_id,
                },
                actor_from(&auth, None),
            )
            .await
            .map_err(ApiError::from)?;
        last_env = envs.into_iter().last().or(last_env);
    }

    Ok(Json(MutationResponse {
        success: true,
        event_id: last_env.as_ref().map(|e| e.id),
        event_seq: last_env.as_ref().map(|e| e.seq),
        data: serde_json::json!({ "document_id": id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

#[derive(Deserialize)]
struct AppendDocumentBody {
    content: String,
}

/// `POST /v1/documents/{id}/append` — append a markdown chunk.
async fn append_document(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
    Json(body): Json<AppendDocumentBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::DocumentWrite)
        .map_err(ApiError::from_missing_cap)?;
    let id = id_str.parse::<DocumentId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid document id: {id_str}"
        )))
    })?;
    let envs = state
        .commands
        .dispatch(
            Command::AppendDocumentContent {
                document_id: id,
                append: body.content,
            },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "document_id": id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

/// `POST /v1/documents/{id}/archive` — soft-archive a document.
async fn archive_document(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::DocumentWrite)
        .map_err(ApiError::from_missing_cap)?;
    let id = id_str.parse::<DocumentId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid document id: {id_str}"
        )))
    })?;
    let envs = state
        .commands
        .dispatch(
            Command::ArchiveDocument { document_id: id },
            actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;
    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: serde_json::json!({ "document_id": id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

#[derive(Deserialize)]
struct ListDocumentsQuery {
    #[serde(default)]
    kind: Option<DocumentKind>,
    /// Defaults to `false` — soft-archived documents are hidden unless the
    /// client opts in.
    #[serde(default)]
    include_archived: bool,
}

/// `GET /v1/projects/{project_id}/documents` — list documents for a project.
async fn list_project_documents(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(project_id_str): Path<String>,
    Query(q): Query<ListDocumentsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::DocumentRead)
        .map_err(ApiError::from_missing_cap)?;
    let project_id = project_id_str.parse::<ProjectId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid project id: {project_id_str}"
        )))
    })?;
    let docs = state
        .documents
        .list_by_project(project_id, q.kind, q.include_archived)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(docs))
}
