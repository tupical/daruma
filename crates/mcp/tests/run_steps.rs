//! Integration tests for run-step tool routing (§3.6 W0.1).
//!
//! Verifies:
//!   1. Both step tools are present in `tool_definitions()`.
//!   2. `daruma_run_start_step` POSTs to `/v1/runs/{id}/step/start`.
//!   3. `daruma_run_finish_step` POSTs to `/v1/runs/{id}/step/finish`
//!      with `{"kind": "done"}` in the outcome object (not `"type"`).

use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, http::StatusCode, routing::post, Router};
use daruma_mcp::tools::call_tool;
use daruma_mcp::{tool_definitions, ApiClient};
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

/// Build an axum `Router` that records the last POST and always returns 200 `{}`.
fn recording_router(capture: Arc<Mutex<Option<Captured>>>) -> Router {
    Router::new().fallback(post(move |req: Request<Body>| {
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

/// Send a single request through the recording router and return what was captured.
async fn call_via_mock(tool: &str, args: Value) -> Captured {
    let captured: Arc<Mutex<Option<Captured>>> = Arc::new(Mutex::new(None));
    let router = recording_router(captured.clone());

    // We use tower `oneshot` so we don't need to bind a real socket.
    // ApiClient needs a base URL, but we intercept at the service level by
    // wrapping reqwest — that would require a real port. Instead, bind a real
    // listener on a random OS port, serve one request, then shut down.
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    // Spawn the server; it will handle exactly one connection then we drop it.
    let router_clone = router.clone();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, router_clone).await.unwrap();
    });

    let client = ApiClient::new(base, "test-token");
    // Ignore the result — mock returns {} which may or may not parse as expected
    let _ = call_tool(&client, tool, args).await;

    // Give the server a brief moment to record the request.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_handle.abort();

    let result = captured
        .lock()
        .unwrap()
        .clone()
        .expect("no request was captured");
    result
}

// ---------------------------------------------------------------------------
// Test 1: catalogue contains both step tools
// ---------------------------------------------------------------------------

#[test]
fn catalogue_contains_step_tools() {
    let names: Vec<&str> = tool_definitions().iter().map(|t| t.name).collect();
    assert!(
        names.contains(&"daruma_run_start_step"),
        "tool_definitions() is missing daruma_run_start_step; found: {names:?}"
    );
    assert!(
        names.contains(&"daruma_run_finish_step"),
        "tool_definitions() is missing daruma_run_finish_step; found: {names:?}"
    );
    assert!(
        names.contains(&"daruma_run_log"),
        "tool_definitions() is missing daruma_run_log; found: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: start_step sends POST /v1/runs/{run_id}/step/start
// ---------------------------------------------------------------------------

#[tokio::test]
async fn start_step_posts_correct_url() {
    let run_id = "run-abc-123";
    let task_id = "task-xyz-456";

    let captured = call_via_mock(
        "daruma_run_start_step",
        json!({ "run_id": run_id, "task_id": task_id }),
    )
    .await;

    assert_eq!(
        captured.path,
        format!("/v1/runs/{run_id}/step/start"),
        "wrong URL for start_step: got {}",
        captured.path
    );
    assert_eq!(
        captured.body["task_id"],
        json!(task_id),
        "body missing task_id"
    );
}

// ---------------------------------------------------------------------------
// Test 3: finish_step sends POST /v1/runs/{run_id}/step/finish with kind field
// ---------------------------------------------------------------------------

#[tokio::test]
async fn finish_step_posts_correct_url_and_kind_field() {
    let run_id = "run-abc-123";
    let task_id = "task-xyz-456";

    let captured = call_via_mock(
        "daruma_run_finish_step",
        json!({
            "run_id": run_id,
            "task_id": task_id,
            "outcome": { "kind": "done" }
        }),
    )
    .await;

    assert_eq!(
        captured.path,
        format!("/v1/runs/{run_id}/step/finish"),
        "wrong URL for finish_step: got {}",
        captured.path
    );
    assert_eq!(
        captured.body["task_id"],
        json!(task_id),
        "body missing task_id"
    );
    assert_eq!(
        captured.body["outcome"]["kind"],
        json!("done"),
        "outcome must use 'kind' field, not 'type'; body was: {}",
        captured.body
    );
    // Ensure the old wrong field name is absent
    assert!(
        captured.body["outcome"].get("type").is_none(),
        "outcome must NOT have a 'type' field; body was: {}",
        captured.body
    );
}

#[tokio::test]
async fn run_log_posts_leveled_body_to_notes_endpoint() {
    let run_id = "run-abc-123";

    let captured = call_via_mock(
        "daruma_run_log",
        json!({
            "run_id": run_id,
            "level": "warn",
            "body": "waiting for worker"
        }),
    )
    .await;

    assert_eq!(
        captured.path,
        format!("/v1/runs/{run_id}/notes"),
        "wrong URL for run_log: got {}",
        captured.path
    );
    assert_eq!(
        captured.body["body"],
        json!("[warn] waiting for worker"),
        "run_log should encode level in the note body"
    );
}
