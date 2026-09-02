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
use daruma_events::Event;
use daruma_shared::RunId;
use serde_json::Value;
use tower::ServiceExt;

mod common;
use common::{mint_pat, test_app};

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

async fn create_active_plan(app: &axum::Router, token: &str) -> String {
    let (status, response) = post_json(
        app,
        token,
        "/v1/commands",
        r#"{"command":{"type":"create_project","title":"Run owner project"}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create project failed: {response}");
    let project_id = response["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|event| {
            let payload = event.get("payload")?;
            (payload.get("type")?.as_str()? == "project_created")
                .then(|| payload["project"]["id"].as_str().unwrap().to_owned())
        })
        .unwrap();
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
async fn authenticated_run_owner_guards_every_terminal_http_ingress() {
    let h = test_app().await;
    let plan_id = create_active_plan(&h.router, &h.admin_token).await;
    let caps: Capabilities = [Capability::RunWrite, Capability::PlanWrite].into();
    let (owner_token, owner_id) = mint_pat(&h.auth_store(), caps.clone(), ProjectFilter::All).await;
    let (foreign_token, foreign_id) = mint_pat(&h.auth_store(), caps, ProjectFilter::All).await;

    // The body agent is deliberately foreign: authenticated token identity
    // must still own the run.
    let (status, response) = post_json(
        &h.router,
        &owner_token,
        "/v1/runs",
        &format!(
            r#"{{"plan_id":"{plan_id}","agent_id":"{}"}}"#,
            foreign_id.as_uuid()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "start run failed: {response}");
    let run_id = response["data"]["run_id"].as_str().unwrap();

    let (status, _) = post_json(
        &h.router,
        &foreign_token,
        &format!("/v1/runs/{run_id}/abort"),
        r#"{"reason":"foreign"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = post_json(
        &h.router,
        &foreign_token,
        "/v1/commands",
        &format!(r#"{{"command":{{"type":"fail_run","run_id":"{run_id}","reason":"foreign"}}}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = post_json(
        &h.router,
        &foreign_token,
        &format!("/v1/plans/{plan_id}/archive"),
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = delete_json(&h.router, &foreign_token, &format!("/v1/plans/{plan_id}")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let run = h
        .state
        .runs
        .get(run_id.parse::<RunId>().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, daruma_domain::RunStatus::Active);

    let (status, response) = post_json(
        &h.router,
        &owner_token,
        &format!("/v1/runs/{run_id}/abort"),
        r#"{"reason":"owner"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "owner abort failed: {response}");
    let run = h
        .state
        .runs
        .get(run_id.parse::<RunId>().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, daruma_domain::RunStatus::Aborted);
    assert_ne!(owner_id, foreign_id);
}

#[tokio::test]
async fn generated_drain_run_is_returned_and_finishable() {
    let h = test_app().await;
    let plan_id = create_active_plan(&h.router, &h.admin_token).await;
    let task_id = create_task(&h.router, &h.admin_token).await;
    let (status, response) = post_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{plan_id}/tasks"),
        &format!(r#"{{"task_id":"{task_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "add task failed: {response}");

    let (status, response) = post_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{plan_id}/drain-next"),
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "drain failed: {response}");
    assert_eq!(response["task_id"], task_id);
    let run_id = response["run_id"]
        .as_str()
        .expect("generated drain must return run_id")
        .parse::<RunId>()
        .unwrap();
    assert_eq!(
        h.state.runs.get(run_id).await.unwrap().unwrap().status,
        daruma_domain::RunStatus::Active
    );

    let (status, response) = post_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/runs/{run_id}/abort"),
        r#"{"reason":"done"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "abort failed: {response}");
}

#[tokio::test]
async fn empty_drain_aborts_its_unreturned_generated_run() {
    let h = test_app().await;
    let plan_id = create_active_plan(&h.router, &h.admin_token).await;

    let (status, response) = post_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{plan_id}/drain-next"),
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "empty drain failed: {response}");
    assert!(response.is_null());

    let events = h.state.store.load_since(0, 100).await.unwrap();
    let run_id = events
        .iter()
        .find_map(|event| match &event.payload {
            Event::RunStarted { run } => Some(run.id),
            _ => None,
        })
        .expect("generated run must be recorded");
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        Event::RunAborted { run_id: aborted, .. } if *aborted == run_id
    )));
    assert_eq!(
        h.state.runs.get(run_id).await.unwrap().unwrap().status,
        daruma_domain::RunStatus::Aborted
    );
}
