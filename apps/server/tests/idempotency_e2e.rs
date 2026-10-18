//! End-to-end integration tests for idempotent command dispatch (W3.1, Linear A.1).
//!
//! Covers:
//!   Same client_command_id sent twice → second call returns cached event_id,
//!     original event data (no new event stored)
//!   Different client_command_id → fresh execution, distinct event_id
//!   No client_command_id → never deduplicated (each call produces a new event)

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

// ── AC: Idempotent dispatch (Linear A.1) ──────────────────────────────────────

#[tokio::test]
async fn idempotency_same_ccid_returns_same_event_id() {
    let h = test_app().await;
    let ccid = uuid::Uuid::new_v4();

    let body = format!(
        r#"{{"command":{{"type":"create_task","task":{{"title":"Idem test"}}}},"client_command_id":"{ccid}"}}"#
    );

    // First call — executes the command and stores the result.
    let (s1, r1) = post_json(&h.router, &h.admin_token, "/v1/commands", &body).await;
    assert_eq!(s1, StatusCode::OK, "first call failed: {r1}");
    assert_eq!(r1["success"], true);
    let event_id_1 = r1["event_id"]
        .as_str()
        .expect("event_id on first call")
        .to_owned();
    assert!(
        r1["data"].is_array(),
        "first call must return events array: {r1}"
    );

    // Second call with the same client_command_id — must return cached result.
    let (s2, r2) = post_json(&h.router, &h.admin_token, "/v1/commands", &body).await;
    assert_eq!(s2, StatusCode::OK, "second call failed: {r2}");
    assert_eq!(r2["success"], true);
    let event_id_2 = r2["event_id"]
        .as_str()
        .expect("event_id on second call")
        .to_owned();

    assert_eq!(
        event_id_1, event_id_2,
        "same client_command_id must return identical event_id"
    );
    assert!(
        r2["data"].is_array(),
        "cached response must restore original event data: {r2}"
    );
    assert_eq!(r2["data"][0]["payload"]["type"], "task_created");
    assert_eq!(
        r2["client_command_id"].as_str().unwrap(),
        ccid.to_string(),
        "client_command_id must be echoed back"
    );
}

#[tokio::test]
async fn idempotency_different_ccid_produces_new_event() {
    let h = test_app().await;
    let ccid_a = uuid::Uuid::new_v4();
    let ccid_b = uuid::Uuid::new_v4();

    let body_a = format!(
        r#"{{"command":{{"type":"create_task","task":{{"title":"Task A"}}}},"client_command_id":"{ccid_a}"}}"#
    );
    let body_b = format!(
        r#"{{"command":{{"type":"create_task","task":{{"title":"Task B"}}}},"client_command_id":"{ccid_b}"}}"#
    );

    let (_, r_a) = post_json(&h.router, &h.admin_token, "/v1/commands", &body_a).await;
    let (_, r_b) = post_json(&h.router, &h.admin_token, "/v1/commands", &body_b).await;

    let eid_a = r_a["event_id"].as_str().expect("event_id A").to_owned();
    let eid_b = r_b["event_id"].as_str().expect("event_id B").to_owned();

    assert_ne!(
        eid_a, eid_b,
        "distinct client_command_ids must produce distinct event_ids"
    );
    assert!(
        r_a["data"].is_array(),
        "first fresh call must return events: {r_a}"
    );
    assert!(
        r_b["data"].is_array(),
        "second fresh call must return events: {r_b}"
    );
}

#[tokio::test]
async fn idempotency_omitted_ccid_is_never_deduplicated() {
    let h = test_app().await;

    // Two identical commands without client_command_id — each creates a new task.
    let body = r#"{"command":{"type":"create_task","task":{"title":"No ccid"}}}"#;

    let (s1, r1) = post_json(&h.router, &h.admin_token, "/v1/commands", body).await;
    let (s2, r2) = post_json(&h.router, &h.admin_token, "/v1/commands", body).await;

    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);

    let eid_1 = r1["event_id"].as_str().expect("event_id 1").to_owned();
    let eid_2 = r2["event_id"].as_str().expect("event_id 2").to_owned();

    assert_ne!(
        eid_1, eid_2,
        "commands without client_command_id must never deduplicate"
    );
    assert!(r1["data"].is_array(), "call 1 must return events");
    assert!(r2["data"].is_array(), "call 2 must return events");
}

// ── AC: concurrent retries execute once (MODULE_CONTRACT §3) ──────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idempotency_concurrent_same_ccid_executes_once() {
    let h = test_app().await;
    let ccid = uuid::Uuid::new_v4();
    let title = format!("Concurrent idem {ccid}");
    let body = format!(
        r#"{{"command":{{"type":"create_task","task":{{"title":"{title}"}}}},"client_command_id":"{ccid}"}}"#
    );

    let calls = (0..8).map(|_| {
        let router = h.router.clone();
        let token = h.admin_token.clone();
        let body = body.clone();
        tokio::spawn(async move { post_json(&router, &token, "/v1/commands", &body).await })
    });
    let mut event_ids = Vec::new();
    for call in calls.collect::<Vec<_>>() {
        let (status, res) = call.await.unwrap();
        assert_eq!(status, StatusCode::OK, "concurrent retry failed: {res}");
        event_ids.push(res["event_id"].as_str().expect("event_id").to_owned());
    }

    event_ids.dedup();
    assert_eq!(
        event_ids.len(),
        1,
        "concurrent retries must share one event_id: {event_ids:?}"
    );
    let (tasks,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tasks WHERE title = ?")
        .bind(&title)
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(tasks, 1, "the mutation must run exactly once");
    let (created,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE kind = 'task_created' AND payload_json LIKE ?",
    )
    .bind(format!("%{title}%"))
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(created, 1, "exactly one task_created event per ccid");
}

#[tokio::test]
async fn idempotency_failed_command_does_not_consume_ccid() {
    let h = test_app().await;
    let ccid = uuid::Uuid::new_v4();

    // A command that fails must release the key instead of pinning it.
    let missing = uuid::Uuid::now_v7();
    let failing = format!(
        r#"{{"command":{{"type":"set_status","id":"{missing}","status":"in_progress"}},"client_command_id":"{ccid}"}}"#
    );
    let (s1, r1) = post_json(&h.router, &h.admin_token, "/v1/commands", &failing).await;
    assert_ne!(
        s1,
        StatusCode::OK,
        "command on a missing task must fail: {r1}"
    );

    let retry = format!(
        r#"{{"command":{{"type":"create_task","task":{{"title":"Retry after failure"}}}},"client_command_id":"{ccid}"}}"#
    );
    let (s2, r2) = post_json(&h.router, &h.admin_token, "/v1/commands", &retry).await;
    assert_eq!(
        s2,
        StatusCode::OK,
        "retry after a failure must execute: {r2}"
    );
    assert_eq!(r2["data"][0]["payload"]["type"], "task_created");
}
