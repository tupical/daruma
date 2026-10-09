//! Integration tests for daruma_plan_update — parent_plan_id support (W3).
//!
//! Verifies:
//!   1. `plan_update_reparent_via_mcp`  — PATCH body carries the new parent_plan_id string.
//!   2. `plan_update_unparent_via_mcp`  — PATCH body carries explicit `null` for unparent.
//!   3. `plan_update_cycle_via_mcp_returns_error` — 422 from server → call_tool returns Err.

use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, http::StatusCode, routing::any, Router};
use daruma_mcp::tools::call_tool;
use daruma_mcp::ApiClient;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Captured HTTP request details from the mock server.
#[derive(Debug, Clone)]
struct Captured {
    path: String,
    body: Value,
}

/// Router that records the last request (any HTTP method) and always returns 200 `{}`.
fn recording_router(capture: Arc<Mutex<Option<Captured>>>) -> Router {
    Router::new().fallback(any(move |req: Request<Body>| {
        let capture = capture.clone();
        async move {
            let path = req.uri().path().to_string();
            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .unwrap_or_default();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            *capture.lock().unwrap() = Some(Captured { path, body });
            (StatusCode::OK, axum::Json(json!({})))
        }
    }))
}

/// Router that always returns a fixed non-2xx status + JSON body (for error tests).
fn error_router(status: StatusCode, error_body: Value) -> Router {
    Router::new().fallback(any(move || {
        let error_body = error_body.clone();
        async move { (status, axum::Json(error_body)) }
    }))
}

/// Spin up a recording server, invoke `call_tool`, return (tool result, captured request).
async fn call_via_recording(tool: &str, args: Value) -> (anyhow::Result<Value>, Captured) {
    use tokio::net::TcpListener;

    let captured: Arc<Mutex<Option<Captured>>> = Arc::new(Mutex::new(None));
    let router = recording_router(captured.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    let server_handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let client = ApiClient::new(base, "test-token");
    let result = call_tool(&client, tool, args).await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_handle.abort();

    let cap = captured
        .lock()
        .unwrap()
        .clone()
        .expect("no request was captured by the mock server");
    (result, cap)
}

/// Spin up an error server, invoke `call_tool`, return the tool result (expected to be Err).
async fn call_via_error_server(
    tool: &str,
    args: Value,
    status: StatusCode,
    body: Value,
) -> anyhow::Result<Value> {
    use tokio::net::TcpListener;

    let router = error_router(status, body);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    let server_handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let client = ApiClient::new(base, "test-token");
    let result = call_tool(&client, tool, args).await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_handle.abort();

    result
}

// ---------------------------------------------------------------------------
// Test 1: reparent — parent_plan_id string is forwarded in the PATCH body
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plan_update_reparent_via_mcp() {
    let plan_id = "plan-aaa-111";
    let parent_id = "plan-bbb-222";

    let (result, captured) = call_via_recording(
        "daruma_plan_update",
        json!({
            "id": plan_id,
            "patch": { "parent_plan_id": parent_id }
        }),
    )
    .await;

    assert!(
        result.is_ok(),
        "call_tool should succeed for reparent; got: {result:?}"
    );
    assert_eq!(
        captured.path,
        format!("/v1/plans/{plan_id}"),
        "wrong URL — expected PATCH /v1/plans/{plan_id}, got {}",
        captured.path
    );
    assert_eq!(
        captured.body["patch"]["parent_plan_id"],
        json!(parent_id),
        "parent_plan_id must be forwarded in the patch body; body was: {}",
        captured.body
    );
}

// ---------------------------------------------------------------------------
// Test 2: unparent — explicit null is preserved in the PATCH body
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plan_update_unparent_via_mcp() {
    let plan_id = "plan-aaa-111";

    let (result, captured) = call_via_recording(
        "daruma_plan_update",
        json!({
            "id": plan_id,
            "patch": { "parent_plan_id": null }
        }),
    )
    .await;

    assert!(
        result.is_ok(),
        "call_tool should succeed for unparent; got: {result:?}"
    );
    assert_eq!(
        captured.path,
        format!("/v1/plans/{plan_id}"),
        "wrong URL — expected PATCH /v1/plans/{plan_id}, got {}",
        captured.path
    );
    // Explicit null MUST be present in the forwarded patch — not absent/dropped.
    // The server uses deserialize_double_option: absent = no change, null = unparent.
    let patch_obj = captured.body["patch"]
        .as_object()
        .expect("patch should be a JSON object");
    assert!(
        patch_obj.contains_key("parent_plan_id"),
        "parent_plan_id must be present (as null) in forwarded patch for unparent; body was: {}",
        captured.body
    );
    assert!(
        captured.body["patch"]["parent_plan_id"].is_null(),
        "parent_plan_id must be JSON null for unparent; body was: {}",
        captured.body
    );
}

// ---------------------------------------------------------------------------
// Test 3: cycle — server validation error propagates as Err from call_tool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plan_update_cycle_via_mcp_returns_error() {
    let plan_id = "plan-aaa-111";
    // Attempt to make a plan its own parent — the server detects a cycle and returns 422.
    let result = call_via_error_server(
        "daruma_plan_update",
        json!({
            "id": plan_id,
            "patch": { "parent_plan_id": plan_id }
        }),
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({ "error": "cycle detected: plan cannot be its own ancestor" }),
    )
    .await;

    assert!(
        result.is_err(),
        "call_tool must return Err when the server signals a cycle/validation error"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("422"),
        "error message should reference the HTTP 422 status; got: {msg}"
    );
}
