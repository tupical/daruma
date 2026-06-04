//! End-to-end tests for the standard MutationResponse shape (W3.1 §3.9).
//!
//! Covers:
//!   POST /v1/commands → success=true, event_id (string), event_seq (number),
//!                       data (array of event envelopes)
//!   POST /v1/plans    → success=true, event_id (string), event_seq (number),
//!                       data.plan_id is present
//!   Idempotent replay → original event envelope restored, event_id/event_seq preserved

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::Value;
use tower::ServiceExt;

mod common;
use common::test_app;

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

async fn get_json(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::GET)
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

/// Create a project and return its UUID string.
async fn create_project(app: &axum::Router, token: &str) -> String {
    let (s, ev) = post_json(
        app,
        token,
        "/v1/commands",
        r#"{"command":{"type":"create_project","title":"Shape Test Project"}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    ev["data"]
        .as_array()
        .expect("data array")
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == "project_created" {
                p.get("project")?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("project_created event")
}

// ── AC: MutationResponse shape on /v1/commands ────────────────────────────────

#[tokio::test]
async fn commands_response_has_required_fields() {
    let h = test_app().await;

    let (s, resp) = post_json(
        &h.router,
        &h.admin_token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"Shape check"}}}"#,
    )
    .await;

    assert_eq!(s, StatusCode::OK, "dispatch must return 200: {resp}");

    // success
    assert_eq!(resp["success"], true, "success must be true: {resp}");

    // event_id — present as a non-empty string
    assert!(
        resp["event_id"].is_string(),
        "event_id must be a string: {resp}"
    );
    assert!(
        !resp["event_id"].as_str().unwrap().is_empty(),
        "event_id must be non-empty: {resp}"
    );

    // event_seq — present as a non-negative integer
    assert!(
        resp["event_seq"].is_number(),
        "event_seq must be a number: {resp}"
    );
    assert!(
        resp["event_seq"].as_u64().is_some(),
        "event_seq must be a non-negative integer: {resp}"
    );

    // data — array with at least one event envelope
    let data = resp["data"].as_array().expect("data must be an array");
    assert!(!data.is_empty(), "data must contain at least one event");

    // Each envelope must have id, seq, payload.type
    let env = &data[0];
    assert!(env["id"].is_string(), "envelope.id must be string");
    assert!(env["seq"].is_number(), "envelope.seq must be number");
    assert!(
        env["payload"]["type"].is_string(),
        "envelope.payload.type must be string"
    );
}

// ── AC: MutationResponse shape on /v1/plans ───────────────────────────────────

#[tokio::test]
async fn plans_create_response_has_required_fields() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;

    let plan_body = format!(
        r#"{{"plan":{{"project_id":"{pid}","title":"Shape check","owner":{{"kind":"user"}}}}}}"#
    );
    let (s, resp) = post_json(&h.router, &h.admin_token, "/v1/plans", &plan_body).await;

    assert_eq!(
        s,
        StatusCode::CREATED,
        "create plan must return 201: {resp}"
    );
    assert_eq!(resp["success"], true);
    assert!(
        resp["event_id"].is_string(),
        "event_id must be present: {resp}"
    );
    assert!(
        resp["event_seq"].is_number(),
        "event_seq must be present: {resp}"
    );
    assert!(
        resp["data"]["plan_id"].is_string(),
        "plan create data must include plan_id: {resp}"
    );
}

// ── AC: Idempotent replay shape ───────────────────────────────────────────────

#[tokio::test]
async fn idempotent_replay_returns_event_data_with_original_event_id() {
    let h = test_app().await;
    let ccid = uuid::Uuid::new_v4();

    let body = format!(
        r#"{{"command":{{"type":"create_task","task":{{"title":"Replay shape"}}}},"client_command_id":"{ccid}"}}"#
    );

    // First call — live execution.
    let (_, r1) = post_json(&h.router, &h.admin_token, "/v1/commands", &body).await;
    let event_id_1 = r1["event_id"]
        .as_str()
        .expect("event_id on first call")
        .to_owned();
    let event_seq_1 = r1["event_seq"].as_u64().expect("event_seq on first call");
    assert!(r1["data"].is_array(), "first call data must be array: {r1}");

    // Second call — cached.
    let (s2, r2) = post_json(&h.router, &h.admin_token, "/v1/commands", &body).await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(r2["success"], true);
    assert_eq!(
        r2["event_id"].as_str().unwrap(),
        event_id_1,
        "cached event_id must match original"
    );
    assert_eq!(
        r2["event_seq"].as_u64().unwrap(),
        event_seq_1,
        "cached event_seq must match original"
    );
    assert!(
        r2["data"].is_array(),
        "cached replay must return original event data: {r2}"
    );
    assert_eq!(r2["data"][0]["payload"]["type"], "task_created");
}

// ── AC: No client_command_id → data is always an array ───────────────────────

#[tokio::test]
async fn commands_without_ccid_always_return_events_array() {
    let h = test_app().await;

    // Two calls without client_command_id — each must return a live data array.
    let body = r#"{"command":{"type":"create_task","task":{"title":"No ccid shape"}}}"#;

    let (s1, r1) = post_json(&h.router, &h.admin_token, "/v1/commands", body).await;
    let (s2, r2) = post_json(&h.router, &h.admin_token, "/v1/commands", body).await;

    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert!(r1["data"].is_array(), "call 1 data must be array: {r1}");
    assert!(r2["data"].is_array(), "call 2 data must be array: {r2}");

    // Each call produces a distinct event.
    let eid1 = r1["event_id"].as_str().unwrap();
    let eid2 = r2["event_id"].as_str().unwrap();
    assert_ne!(
        eid1, eid2,
        "each call without ccid must produce a fresh event"
    );
}

// ── AC: list_events reflects stored events (data integrity smoke test) ────────

#[tokio::test]
async fn list_events_reflects_dispatched_commands() {
    let h = test_app().await;

    let (_, resp) = post_json(
        &h.router,
        &h.admin_token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"Event store check"}}}"#,
    )
    .await;
    let event_seq = resp["event_seq"].as_u64().expect("event_seq");

    // The event should be visible via GET /v1/events?since=0.
    let (s, events) = get_json(&h.router, &h.admin_token, "/v1/events?since=0").await;
    assert_eq!(s, StatusCode::OK);
    let arr = events.as_array().expect("events must be array");
    assert!(
        arr.iter().any(|e| e["seq"].as_u64() == Some(event_seq)),
        "dispatched event must appear in GET /v1/events"
    );
}
