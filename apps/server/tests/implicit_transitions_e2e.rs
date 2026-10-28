//! Implicit lifecycle transitions: status follows the agent's own writes.
//!
//! A work-start write (claim, `intent`/`progress` comment, run step start,
//! evidence, git_context) moves a `todo` task to `in_progress`; an `outcome`
//! comment moves `todo | in_progress` to `in_review`; `blocker`/`research`
//! comments move nothing. A lifecycle rule that blocks the move keeps the
//! status, lets the write through and reports the rule as a warning. claim,
//! release and run start need no `agent_id` — the token names the agent. An
//! expired claim leaves the status alone even after the TTL sweep.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use daruma_auth::{Capabilities, Capability, ProjectFilter};
use daruma_core::lifecycle_gate::{
    GateCheck, GateDecision, GateOverride, LifecycleGate, TriggerEvent,
};
use daruma_domain::Actor;
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::{mint_pat, TestApp, TestAppBuilder};

/// Blocks `task.before_start` while the flag is set.
struct ToggleGate {
    block_before_start: AtomicBool,
}

#[async_trait]
impl LifecycleGate for ToggleGate {
    async fn check(
        &self,
        _actor: &Actor,
        check: &GateCheck,
        _gate_override: &GateOverride,
    ) -> daruma_shared::Result<GateDecision> {
        if check.trigger == TriggerEvent::TaskBeforeStart
            && self.block_before_start.load(Ordering::SeqCst)
        {
            return Ok(GateDecision::Blocked {
                message: "task.before_start requires evidence".to_string(),
                details: Value::Null,
            });
        }
        Ok(GateDecision::Allowed)
    }
}

async fn send(
    app: &axum::Router,
    token: &str,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post(app: &TestApp, token: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&app.router, token, Method::POST, uri, Some(body)).await
}

fn created_id(ev: &Value, event_type: &str, entity: &str) -> String {
    ev["data"]
        .as_array()
        .expect("data array")
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            (p.get("type")?.as_str()? == event_type)
                .then(|| p.get(entity)?.get("id")?.as_str().map(str::to_owned))?
        })
        .unwrap_or_else(|| panic!("{event_type} event with {entity}.id in {ev}"))
}

async fn new_task(app: &TestApp, title: &str) -> String {
    let (s, ev) = post(
        app,
        &app.admin_token,
        "/v1/commands",
        json!({"command": {"type": "create_task", "task": {"title": title}}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{ev}");
    let id = created_id(&ev, "task_created", "task");
    set_status(app, &id, "todo").await;
    id
}

async fn set_status(app: &TestApp, task: &str, status: &str) {
    let (s, r) = post(
        app,
        &app.admin_token,
        "/v1/commands",
        json!({"command": {"type": "set_status", "id": task, "status": status, "force": true}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "set_status {status}: {r}");
}

async fn status_of(app: &TestApp, task: &str) -> String {
    let (s, t) = send(
        &app.router,
        &app.admin_token,
        Method::GET,
        &format!("/v1/tasks/{task}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{t}");
    t["status"].as_str().unwrap().to_owned()
}

async fn comment(app: &TestApp, token: &str, task: &str, kind: &str) -> Value {
    let (s, c) = post(
        app,
        token,
        &format!("/v1/tasks/{task}/comments"),
        json!({"body": format!("{kind} note"), "kind": kind}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{kind} comment: {c}");
    c
}

#[tokio::test]
async fn work_start_comments_move_todo_to_in_progress() {
    let app = TestAppBuilder::default().build().await;
    for kind in ["intent", "progress"] {
        let task = new_task(&app, kind).await;
        let c = comment(&app, &app.admin_token, &task, kind).await;
        assert!(c.get("warnings").is_none(), "{c}");
        assert_eq!(status_of(&app, &task).await, "in_progress", "{kind}");
    }
}

#[tokio::test]
async fn outcome_comment_moves_to_in_review_and_others_do_not_move() {
    let app = TestAppBuilder::default().build().await;

    let working = new_task(&app, "working").await;
    set_status(&app, &working, "in_progress").await;
    comment(&app, &app.admin_token, &working, "outcome").await;
    assert_eq!(status_of(&app, &working).await, "in_review");

    let fresh = new_task(&app, "fresh").await;
    comment(&app, &app.admin_token, &fresh, "outcome").await;
    assert_eq!(status_of(&app, &fresh).await, "in_review");

    for kind in ["blocker", "research"] {
        let task = new_task(&app, kind).await;
        comment(&app, &app.admin_token, &task, kind).await;
        assert_eq!(status_of(&app, &task).await, "todo", "{kind}");
    }
    // A plain comment and a work-start comment on a reviewed task move nothing.
    let plain = new_task(&app, "plain").await;
    let (s, _) = post(
        &app,
        &app.admin_token,
        &format!("/v1/tasks/{plain}/comments"),
        json!({"body": "no kind"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(status_of(&app, &plain).await, "todo");
    comment(&app, &app.admin_token, &working, "intent").await;
    assert_eq!(status_of(&app, &working).await, "in_review");
}

#[tokio::test]
async fn claim_without_agent_id_starts_task_and_release_me_frees_it() {
    let app = TestAppBuilder::default().build().await;
    let caps: Capabilities = [
        Capability::TaskRead,
        Capability::TaskWrite,
        Capability::RunWrite,
    ]
    .into();
    let (token, agent) = mint_pat(&app.auth_store(), caps, ProjectFilter::All).await;
    let task = new_task(&app, "claimed").await;

    let (s, r) = post(
        &app,
        &token,
        "/v1/claims",
        json!({"task_id": task, "ttl_secs": 600}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["data"]["acquired"], true, "{r}");
    assert_eq!(r["data"]["agent_id"], agent.as_uuid().to_string(), "{r}");
    assert_eq!(status_of(&app, &task).await, "in_progress");

    let (s, r) = send(
        &app.router,
        &token,
        Method::DELETE,
        &format!("/v1/claims/me/{task}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["data"]["agent_id"], agent.as_uuid().to_string(), "{r}");
    let (_, t) = send(
        &app.router,
        &token,
        Method::GET,
        &format!("/v1/tasks/{task}"),
        None,
    )
    .await;
    assert!(t["current_claim"].is_null(), "{t}");
    // Releasing keeps the status: in_progress is the agent's last word.
    assert_eq!(t["status"], "in_progress");
}

#[tokio::test]
async fn caller_without_task_write_does_not_move_status() {
    let app = TestAppBuilder::default().build().await;
    let caps: Capabilities = [Capability::TaskRead, Capability::RunWrite].into();
    let (token, _) = mint_pat(&app.auth_store(), caps, ProjectFilter::All).await;
    let task = new_task(&app, "read-only").await;
    let (s, r) = post(
        &app,
        &token,
        "/v1/claims",
        json!({"task_id": task, "ttl_secs": 600}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(status_of(&app, &task).await, "todo");
}

#[tokio::test]
async fn run_step_evidence_and_git_context_start_the_task() {
    let app = TestAppBuilder::default().build().await;
    let admin = app.admin_token.clone();

    // Run step: an active plan, a run opened without agent_id.
    let task = new_task(&app, "stepped").await;
    let (s, ev) = post(
        &app,
        &admin,
        "/v1/commands",
        json!({"command": {"type": "create_project", "title": "Lifecycle"}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{ev}");
    let pid = created_id(&ev, "project_created", "project");
    let (s, r) = post(
        &app,
        &admin,
        "/v1/plans",
        json!({"plan": {"project_id": pid, "title": "Steps", "owner": {"kind": "user"}}}),
    )
    .await;
    assert!(s.is_success(), "{r}");
    let (_, list) = send(
        &app.router,
        &admin,
        Method::GET,
        &format!("/v1/plans?project_id={pid}&status=all"),
        None,
    )
    .await;
    let plan = list[0]["id"].as_str().unwrap().to_owned();
    let (s, r) = post(
        &app,
        &admin,
        &format!("/v1/plans/{plan}/tasks"),
        json!({"task_id": task}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (s, r) = post(
        &app,
        &admin,
        &format!("/v1/plans/{plan}/status"),
        json!({"status": "active"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(status_of(&app, &task).await, "todo");
    let (s, r) = post(&app, &admin, "/v1/runs", json!({"plan_id": plan})).await;
    assert_eq!(s, StatusCode::CREATED, "run without agent_id: {r}");
    let run = r["data"]["run_id"].as_str().unwrap().to_owned();
    let (s, r) = post(
        &app,
        &admin,
        &format!("/v1/runs/{run}/step/start"),
        json!({"task_id": task}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(status_of(&app, &task).await, "in_progress");

    // Evidence scoped to the task.
    let evidenced = new_task(&app, "evidenced").await;
    let (s, r) = post(
        &app,
        &admin,
        "/v1/evidence",
        json!({"evidence": {"kind": "artifact_created", "scope": {"kind": "task", "id": evidenced}}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(status_of(&app, &evidenced).await, "in_progress");

    // git_context attached; clearing it is not a start signal.
    let located = new_task(&app, "located").await;
    let (s, r) = post(
        &app,
        &admin,
        "/v1/commands",
        json!({"command": {"type": "update_task", "id": located, "patch": {"git_context": null}}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(status_of(&app, &located).await, "todo");
    let (s, r) = post(
        &app,
        &admin,
        "/v1/commands",
        json!({"command": {"type": "update_task", "id": located,
            "patch": {"git_context": {"branch": "work/located"}}}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(status_of(&app, &located).await, "in_progress");
}

#[tokio::test]
async fn blocked_rule_keeps_status_and_lets_the_write_through() {
    let gate = Arc::new(ToggleGate {
        block_before_start: AtomicBool::new(true),
    });
    let app = TestAppBuilder::default()
        .lifecycle_gate(gate.clone())
        .build()
        .await;
    let task = new_task(&app, "gated").await;

    let c = comment(&app, &app.admin_token, &task, "intent").await;
    let warnings = c["warnings"].as_array().expect("warnings").clone();
    assert_eq!(warnings.len(), 1, "{c}");
    assert_eq!(warnings[0]["code"], "implicit_status_blocked", "{c}");
    assert!(
        warnings[0]["message"]
            .as_str()
            .unwrap()
            .contains("task.before_start requires evidence"),
        "{c}"
    );
    assert_eq!(status_of(&app, &task).await, "todo");

    let (s, r) = post(
        &app,
        &app.admin_token,
        "/v1/claims",
        json!({"task_id": task, "ttl_secs": 600}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["data"]["acquired"], true, "claim stands: {r}");
    assert_eq!(r["warnings"][0]["code"], "implicit_status_blocked", "{r}");
    assert_eq!(status_of(&app, &task).await, "todo");

    gate.block_before_start.store(false, Ordering::SeqCst);
    comment(&app, &app.admin_token, &task, "progress").await;
    assert_eq!(status_of(&app, &task).await, "in_progress");
}

#[tokio::test]
async fn expired_claim_leaves_status_after_sweep() {
    let app = TestAppBuilder::default().build().await;
    let task = new_task(&app, "abandoned").await;
    let (s, r) = post(
        &app,
        &app.admin_token,
        "/v1/claims",
        json!({"task_id": task, "ttl_secs": 1}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(status_of(&app, &task).await, "in_progress");

    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
    let swept = app
        .state
        .commands
        .handler()
        .sweep_expired_claims(Actor::user())
        .await
        .unwrap();
    assert_eq!(swept.len(), 1, "the expired claim is swept");
    let (_, t) = send(
        &app.router,
        &app.admin_token,
        Method::GET,
        &format!("/v1/tasks/{task}"),
        None,
    )
    .await;
    assert!(t["current_claim"].is_null(), "{t}");
    assert_eq!(t["status"], "in_progress", "expiry never moves status");
}
