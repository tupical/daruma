//! End-to-end integration tests for Claim HTTP endpoints (W3.1).
//!
//! Covers:
//!   POST   /v1/claims                      → acquire claim (success, MutationResponse)
//!   DELETE /v1/claims/{agent_id}/{task_id} → release claim (success, MutationResponse)
//!   Capability gating: RunWrite enforced on both endpoints

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use daruma_auth::{Capabilities, Capability, ProjectFilter};
use daruma_core::Command;
use daruma_domain::Actor;
use daruma_events::Event;
use daruma_shared::{AgentId, PlanId, RunId, TaskId};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tower::ServiceExt;

mod common;
use common::{mint_pat, spawn_server, test_app};

// ── HTTP helpers ──────────────────────────────────────────────────────────────

async fn post_json(app: &axum::Router, token: &str, uri: &str, body: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn delete_json(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::DELETE)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Create a task and return its UUID string (raw UUID, no `tsk_` prefix).
async fn create_task(app: &axum::Router, token: &str) -> String {
    let (s, ev) = post_json(
        app,
        token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"Claim target"}}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "create_task failed: {ev}");
    ev["data"]
        .as_array()
        .expect("data must be array")
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == "task_created" {
                p.get("task")?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("task_created event with task.id")
}

async fn create_project(app: &axum::Router, token: &str) -> String {
    let (status, response) = post_json(
        app,
        token,
        "/v1/commands",
        r#"{"command":{"type":"create_project","title":"Run owner project"}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create project failed: {response}");
    response["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|event| {
            let payload = event.get("payload")?;
            (payload.get("type")?.as_str()? == "project_created")
                .then(|| payload["project"]["id"].as_str().unwrap().to_owned())
        })
        .unwrap()
}

async fn create_active_plan_in_project(
    app: &axum::Router,
    token: &str,
    project_id: &str,
) -> String {
    let (status, response) = post_json(
        app,
        token,
        "/v1/commands",
        &format!(
            r#"{{"command":{{"type":"create_plan","plan":{{"project_id":"{project_id}","title":"Run owner plan","owner":{{"kind":"user"}}}}}}}}"#
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create plan failed: {response}");
    let plan_id = response["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|event| {
            let payload = event.get("payload")?;
            (payload.get("type")?.as_str()? == "plan_created")
                .then(|| payload["plan"]["id"].as_str().unwrap().to_owned())
        })
        .unwrap();
    let (status, response) = post_json(
        app,
        token,
        &format!("/v1/plans/{plan_id}/status"),
        r#"{"status":"active"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "activate plan failed: {response}");
    plan_id
}

async fn create_active_plan(app: &axum::Router, token: &str) -> String {
    let project_id = create_project(app, token).await;
    create_active_plan_in_project(app, token, &project_id).await
}

async fn attach_task(app: &axum::Router, token: &str, plan_id: &str, task_id: &str) {
    let (status, response) = post_json(
        app,
        token,
        &format!("/v1/plans/{plan_id}/tasks"),
        &format!(r#"{{"task_id":"{task_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "add task failed: {response}");
}

async fn start_run(
    app: &axum::Router,
    token: &str,
    plan_id: &str,
    body_agent_id: AgentId,
) -> RunId {
    let (status, response) = post_json(
        app,
        token,
        "/v1/runs",
        &format!(
            r#"{{"plan_id":"{plan_id}","agent_id":"{}"}}"#,
            body_agent_id.as_uuid()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "start run failed: {response}");
    response["data"]["run_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

async fn assert_no_claim(h: &common::TestApp, task_id: &str) {
    assert!(h
        .state
        .claims
        .get_agents_claiming_task(task_id.parse::<TaskId>().unwrap())
        .await
        .unwrap()
        .is_empty());
}

async fn drain_project(
    app: &axum::Router,
    token: &str,
    project_id: &str,
    run_id: RunId,
) -> (StatusCode, Value) {
    post_json(
        app,
        token,
        &format!("/v1/ready/drain?project_id={project_id}"),
        &format!(r#"{{"run_id":"{}"}}"#, run_id.as_uuid()),
    )
    .await
}

// ── AC: Claim acquire / release ───────────────────────────────────────────────

#[tokio::test]
async fn claims_acquire_returns_mutation_response() {
    let h = test_app().await;
    let task_id = create_task(&h.router, &h.admin_token).await;
    let agent_id = uuid::Uuid::new_v4().to_string();

    let body = format!(r#"{{"agent_id":"{agent_id}","task_id":"{task_id}","ttl_secs":60}}"#);
    let (s, resp) = post_json(&h.router, &h.admin_token, "/v1/claims", &body).await;

    assert_eq!(s, StatusCode::OK, "acquire claim must return 200: {resp}");
    assert_eq!(resp["success"], true);
    assert!(
        resp["event_id"].is_string(),
        "event_id must be present: {resp}"
    );
    let claim_id = resp["data"]["claim_id"]
        .as_str()
        .expect("claim response must expose its generation")
        .parse::<daruma_shared::ClaimId>()
        .expect("claim response generation must be a UUID");
    let events = h.state.store.load_since(0, 100).await.unwrap();
    assert!(events.iter().any(|env| matches!(
        &env.payload,
        Event::AgentClaimed {
            claim_id: Some(event_claim_id),
            ..
        } if *event_claim_id == claim_id
    )));
}

#[tokio::test]
async fn claims_acquire_and_release() {
    let h = test_app().await;
    let task_id = create_task(&h.router, &h.admin_token).await;
    let agent_id = uuid::Uuid::new_v4().to_string();

    // Acquire.
    let acquire_body =
        format!(r#"{{"agent_id":"{agent_id}","task_id":"{task_id}","ttl_secs":60}}"#);
    let (s, resp) = post_json(&h.router, &h.admin_token, "/v1/claims", &acquire_body).await;
    assert_eq!(s, StatusCode::OK, "acquire failed: {resp}");
    assert_eq!(resp["success"], true);

    // Release.
    let (rs, rel_resp) = delete_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/claims/{agent_id}/{task_id}"),
    )
    .await;
    assert_eq!(
        rs,
        StatusCode::OK,
        "release claim must return 200: {rel_resp}"
    );
    assert_eq!(rel_resp["success"], true);
}

// ── AC: Capability gating ─────────────────────────────────────────────────────

#[tokio::test]
async fn claims_acquire_requires_run_write_capability() {
    let h = test_app().await;
    let task_id = create_task(&h.router, &h.admin_token).await;
    let agent_id = uuid::Uuid::new_v4().to_string();

    let (no_run_write, _) = mint_pat(
        &h.auth_store(),
        [Capability::TaskRead].into(),
        ProjectFilter::All,
    )
    .await;
    let body = format!(r#"{{"agent_id":"{agent_id}","task_id":"{task_id}","ttl_secs":30}}"#);
    let (s, resp) = post_json(&h.router, &no_run_write, "/v1/claims", &body).await;

    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "token without run:write must be forbidden: {resp}"
    );
    assert_eq!(resp["error"]["code"], "forbidden");
}

#[tokio::test]
async fn claims_release_requires_run_write_capability() {
    let h = test_app().await;
    let task_id = create_task(&h.router, &h.admin_token).await;
    let agent_id = uuid::Uuid::new_v4().to_string();

    // Acquire with admin token.
    let acquire_body =
        format!(r#"{{"agent_id":"{agent_id}","task_id":"{task_id}","ttl_secs":60}}"#);
    post_json(&h.router, &h.admin_token, "/v1/claims", &acquire_body).await;

    // Attempt release without run:write.
    let (no_run_write, _) = mint_pat(
        &h.auth_store(),
        [Capability::TaskRead].into(),
        ProjectFilter::All,
    )
    .await;
    let (s, resp) = delete_json(
        &h.router,
        &no_run_write,
        &format!("/v1/claims/{agent_id}/{task_id}"),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "token without run:write must be forbidden on release: {resp}"
    );
}

#[tokio::test]
async fn drain_rejects_ownerless_and_foreign_runs_without_claim() {
    let h = test_app().await;
    let plan_id = create_active_plan(&h.router, &h.admin_token).await;
    let caps: Capabilities = [Capability::PlanRead, Capability::RunWrite].into();
    let (owner_token, owner_id) = mint_pat(&h.auth_store(), caps.clone(), ProjectFilter::All).await;
    let (foreign_token, foreign_id) = mint_pat(&h.auth_store(), caps, ProjectFilter::All).await;

    let ownerless_task = create_task(&h.router, &h.admin_token).await;
    attach_task(&h.router, &h.admin_token, &plan_id, &ownerless_task).await;
    let ownerless = h
        .state
        .commands
        .dispatch(
            Command::StartRun {
                plan_id: plan_id.parse::<PlanId>().unwrap(),
                agent_id: owner_id,
                parent_run_id: None,
            },
            Actor::user(),
        )
        .await
        .unwrap()
        .into_iter()
        .find_map(|event| match event.payload {
            Event::RunStarted { run } => Some(run.id),
            _ => None,
        })
        .unwrap();
    let (status, response) = post_json(
        &h.router,
        &owner_token,
        &format!("/v1/plans/{plan_id}/drain-next"),
        &format!(r#"{{"run_id":"{}"}}"#, ownerless.as_uuid()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "ownerless run: {response}");
    assert_no_claim(&h, &ownerless_task).await;

    let foreign_task = create_task(&h.router, &h.admin_token).await;
    attach_task(&h.router, &h.admin_token, &plan_id, &foreign_task).await;
    let owned = start_run(&h.router, &owner_token, &plan_id, foreign_id).await;
    let (status, _) = post_json(
        &h.router,
        &foreign_token,
        &format!("/v1/plans/{plan_id}/drain-next"),
        &format!(r#"{{"run_id":"{}"}}"#, owned.as_uuid()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_no_claim(&h, &foreign_task).await;

    let (status, response) = post_json(
        &h.router,
        &owner_token,
        &format!("/v1/plans/{plan_id}/drain-next"),
        &format!(r#"{{"run_id":"{}"}}"#, owned.as_uuid()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "owner drain failed: {response}");
    assert_eq!(response["task_id"], ownerless_task);
    assert_eq!(response["run_id"], owned.as_uuid().to_string());
    assert_ne!(owner_id, foreign_id);
}

#[tokio::test]
async fn supplied_and_generated_drain_success_return_persisted_run_id() {
    let h = test_app().await;

    let supplied_plan = create_active_plan(&h.router, &h.admin_token).await;
    let supplied_task = create_task(&h.router, &h.admin_token).await;
    attach_task(&h.router, &h.admin_token, &supplied_plan, &supplied_task).await;
    let supplied_run = start_run(&h.router, &h.admin_token, &supplied_plan, AgentId::new()).await;
    let (status, response) = post_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{supplied_plan}/drain-next"),
        &format!(r#"{{"run_id":"{}"}}"#, supplied_run.as_uuid()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "supplied drain failed: {response}");
    assert_eq!(response["run_id"], supplied_run.as_uuid().to_string());

    let generated_plan = create_active_plan(&h.router, &h.admin_token).await;
    let generated_task = create_task(&h.router, &h.admin_token).await;
    attach_task(&h.router, &h.admin_token, &generated_plan, &generated_task).await;
    let (status, response) = post_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{generated_plan}/drain-next"),
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "generated drain failed: {response}");
    let generated_run = response["run_id"]
        .as_str()
        .expect("generated drain must return run_id")
        .parse::<RunId>()
        .unwrap();
    assert_eq!(
        h.state
            .runs
            .get(generated_run)
            .await
            .unwrap()
            .unwrap()
            .status,
        daruma_domain::RunStatus::Active
    );
}

#[tokio::test]
async fn drain_rejects_missing_wrong_plan_inactive_plan_and_terminal_runs_without_claims() {
    let h = test_app().await;
    let caps: Capabilities = [Capability::PlanRead, Capability::RunWrite].into();
    let (owner_token, owner_id) = mint_pat(&h.auth_store(), caps, ProjectFilter::All).await;

    let missing_plan = create_active_plan(&h.router, &h.admin_token).await;
    let missing_task = create_task(&h.router, &h.admin_token).await;
    attach_task(&h.router, &h.admin_token, &missing_plan, &missing_task).await;
    let (status, _) = post_json(
        &h.router,
        &owner_token,
        &format!("/v1/plans/{missing_plan}/drain-next"),
        &format!(r#"{{"run_id":"{}"}}"#, RunId::new().as_uuid()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_no_claim(&h, &missing_task).await;

    let wrong_plan = create_active_plan(&h.router, &h.admin_token).await;
    let wrong_task = create_task(&h.router, &h.admin_token).await;
    attach_task(&h.router, &h.admin_token, &wrong_plan, &wrong_task).await;
    let other_plan = create_active_plan(&h.router, &h.admin_token).await;
    let other_run = start_run(&h.router, &owner_token, &other_plan, owner_id).await;
    let (status, _) = post_json(
        &h.router,
        &owner_token,
        &format!("/v1/plans/{wrong_plan}/drain-next"),
        &format!(r#"{{"run_id":"{}"}}"#, other_run.as_uuid()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_no_claim(&h, &wrong_task).await;

    let inactive_plan = create_active_plan(&h.router, &h.admin_token).await;
    let inactive_task = create_task(&h.router, &h.admin_token).await;
    attach_task(&h.router, &h.admin_token, &inactive_plan, &inactive_task).await;
    let inactive_run = start_run(&h.router, &owner_token, &inactive_plan, owner_id).await;
    let (status, response) = post_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{inactive_plan}/status"),
        r#"{"status":"abandoned"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "abandon plan failed: {response}");
    let (status, _) = post_json(
        &h.router,
        &owner_token,
        &format!("/v1/plans/{inactive_plan}/drain-next"),
        &format!(r#"{{"run_id":"{}"}}"#, inactive_run.as_uuid()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_no_claim(&h, &inactive_task).await;

    let terminal_plan = create_active_plan(&h.router, &h.admin_token).await;
    let terminal_task = create_task(&h.router, &h.admin_token).await;
    attach_task(&h.router, &h.admin_token, &terminal_plan, &terminal_task).await;
    let terminal_run = start_run(&h.router, &owner_token, &terminal_plan, owner_id).await;
    let (status, response) = post_json(
        &h.router,
        &owner_token,
        &format!("/v1/runs/{terminal_run}/abort"),
        r#"{"reason":"test"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "abort run failed: {response}");
    let (status, _) = post_json(
        &h.router,
        &owner_token,
        &format!("/v1/plans/{terminal_plan}/drain-next"),
        &format!(r#"{{"run_id":"{}"}}"#, terminal_run.as_uuid()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_no_claim(&h, &terminal_task).await;
}

#[tokio::test]
async fn project_drain_accepts_supplied_run_for_later_active_plan() {
    let h = test_app().await;
    let project_id = create_project(&h.router, &h.admin_token).await;
    let first_plan = create_active_plan_in_project(&h.router, &h.admin_token, &project_id).await;
    let first_task = create_task(&h.router, &h.admin_token).await;
    attach_task(&h.router, &h.admin_token, &first_plan, &first_task).await;
    let later_plan = create_active_plan_in_project(&h.router, &h.admin_token, &project_id).await;
    let later_task = create_task(&h.router, &h.admin_token).await;
    attach_task(&h.router, &h.admin_token, &later_plan, &later_task).await;
    let caps: Capabilities = [Capability::PlanRead, Capability::RunWrite].into();
    let (owner_token, owner_id) = mint_pat(&h.auth_store(), caps, ProjectFilter::All).await;
    let run_id = start_run(&h.router, &owner_token, &later_plan, owner_id).await;

    let (status, response) = drain_project(&h.router, &owner_token, &project_id, run_id).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "later-plan drain failed: {response}"
    );
    assert_eq!(response["task_id"], later_task);
    assert_eq!(response["plan_id"], later_plan);
    assert_eq!(response["run_id"], run_id.as_uuid().to_string());
    assert_no_claim(&h, &first_task).await;
}

#[tokio::test]
async fn project_drain_rejects_invalid_supplied_runs_before_plan_iteration() {
    let h = test_app().await;
    let caps: Capabilities = [Capability::PlanRead, Capability::RunWrite].into();
    let (owner_token, owner_id) = mint_pat(&h.auth_store(), caps, ProjectFilter::All).await;

    let empty_project = create_project(&h.router, &h.admin_token).await;
    let (status, _) = drain_project(&h.router, &owner_token, &empty_project, RunId::new()).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "missing run must be rejected"
    );

    let foreign_project = create_project(&h.router, &h.admin_token).await;
    let foreign_plan =
        create_active_plan_in_project(&h.router, &h.admin_token, &foreign_project).await;
    let foreign_task = create_task(&h.router, &h.admin_token).await;
    attach_task(&h.router, &h.admin_token, &foreign_plan, &foreign_task).await;
    let foreign_run = start_run(&h.router, &owner_token, &foreign_plan, owner_id).await;
    let requested_project = create_project(&h.router, &h.admin_token).await;
    let (status, _) = drain_project(&h.router, &owner_token, &requested_project, foreign_run).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "wrong-project run must be rejected"
    );
    assert_no_claim(&h, &foreign_task).await;

    let inactive_project = create_project(&h.router, &h.admin_token).await;
    let inactive_plan =
        create_active_plan_in_project(&h.router, &h.admin_token, &inactive_project).await;
    let inactive_task = create_task(&h.router, &h.admin_token).await;
    attach_task(&h.router, &h.admin_token, &inactive_plan, &inactive_task).await;
    let inactive_run = start_run(&h.router, &owner_token, &inactive_plan, owner_id).await;
    let (status, response) = post_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{inactive_plan}/status"),
        r#"{"status":"abandoned"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "abandon plan failed: {response}");
    let (status, _) = drain_project(&h.router, &owner_token, &inactive_project, inactive_run).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "inactive-plan run must be rejected"
    );
    assert_no_claim(&h, &inactive_task).await;

    let terminal_project = create_project(&h.router, &h.admin_token).await;
    let terminal_plan =
        create_active_plan_in_project(&h.router, &h.admin_token, &terminal_project).await;
    let terminal_task = create_task(&h.router, &h.admin_token).await;
    attach_task(&h.router, &h.admin_token, &terminal_plan, &terminal_task).await;
    let terminal_run = start_run(&h.router, &owner_token, &terminal_plan, owner_id).await;
    let (status, response) = post_json(
        &h.router,
        &owner_token,
        &format!("/v1/runs/{terminal_run}/abort"),
        r#"{"reason":"test"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "abort run failed: {response}");
    let (status, _) = drain_project(&h.router, &owner_token, &terminal_project, terminal_run).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "terminal run must be rejected"
    );
    assert_no_claim(&h, &terminal_task).await;
}

#[tokio::test]
async fn websocket_start_run_persists_authenticated_owner_for_drain() {
    let h = test_app().await;
    let plan_id = create_active_plan(&h.router, &h.admin_token).await;
    let task_id = create_task(&h.router, &h.admin_token).await;
    attach_task(&h.router, &h.admin_token, &plan_id, &task_id).await;
    let addr = spawn_server(&h).await;
    let body_agent_id = AgentId::new();
    assert_ne!(body_agent_id, h.admin_agent_id);

    let (mut ws, _) = connect_async(format!("ws://{addr}/v1/ws?token={}", h.admin_token))
        .await
        .unwrap();
    ws.next().await.unwrap().unwrap(); // hello
    ws.send(Message::Text(
        json!({
            "type": "dispatch",
            "command": {
                "type": "start_run",
                "plan_id": plan_id,
                "agent_id": body_agent_id,
                "parent_run_id": null
            }
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for StartRun ack")
            .unwrap()
            .unwrap();
        let Message::Text(text) = frame else { continue };
        let frame: Value = serde_json::from_str(&text).unwrap();
        match frame["type"].as_str() {
            Some("ack") => break,
            Some("error") => panic!("StartRun failed: {frame}"),
            _ => {}
        }
    }

    let run = h
        .state
        .runs
        .list_active_for_plan(plan_id.parse().unwrap())
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let (status, response) = post_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{plan_id}/drain-next"),
        &format!(r#"{{"run_id":"{}"}}"#, run.id.as_uuid()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "owner drain failed: {response}");
    assert_eq!(response["task_id"], task_id);
    assert_eq!(response["run_id"], run.id.as_uuid().to_string());
}
