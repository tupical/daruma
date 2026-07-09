//! Integration test for `daruma_ai_analyze_complexity` (§3.8.3).
//!
//! Verifies:
//!   1. The tool is present in `tool_definitions()`.
//!   2. The dispatch arm POSTs to `/v1/ai/analyze-complexity/{plan_id}`
//!      with an empty body (server pulls task list from storage).

use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, http::StatusCode, routing::post, Router};
use daruma_mcp::tools::call_tool;
use daruma_mcp::{tool_definitions, ApiClient};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
struct Captured {
    path: String,
    body: Value,
}

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
            (StatusCode::OK, axum::Json(json!({"hints": []})))
        }
    }))
}

async fn call_via_mock(tool: &str, args: Value) -> Captured {
    let captured: Arc<Mutex<Option<Captured>>> = Arc::new(Mutex::new(None));
    let router = recording_router(captured.clone());

    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    let server_handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let client = ApiClient::new(base, "test-token");
    let _ = call_tool(&client, tool, args).await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_handle.abort();

    let result = captured
        .lock()
        .unwrap()
        .clone()
        .expect("no request was captured");
    result
}

#[test]
fn catalogue_contains_ai_analyze_complexity() {
    let names: Vec<&str> = tool_definitions().iter().map(|t| t.name).collect();
    assert!(
        names.contains(&"daruma_ai_analyze_complexity"),
        "tool_definitions() missing daruma_ai_analyze_complexity; got: {names:?}"
    );
}

#[tokio::test]
async fn analyze_complexity_posts_to_plan_scoped_url() {
    let plan_id = "pln_test_abc";
    let captured = call_via_mock(
        "daruma_ai_analyze_complexity",
        json!({ "plan_id": plan_id }),
    )
    .await;

    assert_eq!(
        captured.path,
        format!("/v1/ai/analyze-complexity/{plan_id}"),
        "wrong URL: got {}",
        captured.path
    );
    // No client-side body required — server derives the task list from the plan.
    assert_eq!(captured.body, json!({}));
}
