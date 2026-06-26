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
use serde_json::Value;
use daruma_auth::{Capability, ProjectFilter};
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
