//! Plan-only intake на MCP-поверхности (ADR-0007, item 3 разбивки).
//!
//! Verifies:
//!   1. `create_paths_return_bridge_error` — daruma_create / daruma_capture /
//!      daruma_capture_batch убраны: мост называет замену, HTTP не дёргается.
//!   2. `plan_materialize_posts_materialize_plan_command` — daruma_plan_materialize
//!      шлёт POST /v1/commands с {"type":"materialize_plan", plan, tasks}.
//!   3. `plan_materialize_requires_tasks` — пустой/отсутствующий `tasks` — ошибка
//!      до какого-либо запроса.

use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, http::StatusCode, routing::any, Router};
use daruma_mcp::tools::call_tool;
use daruma_mcp::ApiClient;
use serde_json::{json, Value};

#[derive(Debug, Clone)]
struct Captured {
    path: String,
    body: Value,
}

/// Router that records every request (any HTTP method) and returns 200 `{}`.
fn recording_router(capture: Arc<Mutex<Vec<Captured>>>) -> Router {
    Router::new().fallback(any(move |req: Request<Body>| {
        let capture = capture.clone();
        async move {
            let path = req.uri().path().to_string();
            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .unwrap_or_default();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            capture.lock().unwrap().push(Captured { path, body });
            (StatusCode::OK, axum::Json(json!({})))
        }
    }))
}

async fn with_recording_server(
    tool: &str,
    args: Value,
) -> (anyhow::Result<Value>, Vec<Captured>) {
    use tokio::net::TcpListener;

    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
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

    let caps = captured.lock().unwrap().clone();
    (result, caps)
}

#[tokio::test]
async fn create_paths_return_bridge_error() {
    for (tool, args) in [
        ("daruma_create", json!({"task": {"title": "x"}})),
        ("daruma_capture", json!({"text": "x"})),
        ("daruma_capture_batch", json!({"texts": ["x"]})),
    ] {
        let (result, captured) = with_recording_server(tool, args).await;
        let err = result.expect_err(&format!("{tool} must be bridged"));
        let msg = err.to_string();
        assert!(msg.contains("plan_only_intake"), "{tool}: {msg}");
        assert!(msg.contains("daruma_plan_materialize"), "{tool}: {msg}");
        assert!(
            captured.is_empty(),
            "{tool} must not reach the server, got {captured:?}"
        );
    }
}

#[tokio::test]
async fn plan_materialize_posts_materialize_plan_command() {
    let (result, captured) = with_recording_server(
        "daruma_plan_materialize",
        json!({
            "plan": {"title": "Wave 1", "project_id": "prj-1", "goal": "ship"},
            "tasks": [
                {"title": "step 1"},
                {"title": "step 2", "priority": "p1"},
            ],
        }),
    )
    .await;
    result.expect("materialize must succeed against 200 {}");

    let cap = captured
        .iter()
        .find(|c| c.path == "/v1/commands")
        .expect("materialize must POST /v1/commands");
    let command = &cap.body["command"];
    assert_eq!(command["type"], "materialize_plan", "{command}");
    assert_eq!(command["plan"]["title"], "Wave 1");
    assert_eq!(command["plan"]["project_id"], "prj-1");
    assert_eq!(command["plan"]["goal"], "ship");
    assert_eq!(command["plan"]["owner"]["kind"], "user");
    let tasks = command["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0]["title"], "step 1");
    assert_eq!(tasks[1]["priority"], "p1");
}

#[tokio::test]
async fn plan_materialize_requires_tasks() {
    for args in [
        json!({"plan": {"title": "no tasks", "project_id": "prj-1"}}),
        json!({"plan": {"title": "empty", "project_id": "prj-1"}, "tasks": []}),
    ] {
        let (result, captured) = with_recording_server("daruma_plan_materialize", args).await;
        let err = result.expect_err("tasks are required");
        assert!(err.to_string().contains("tasks"), "{err}");
        assert!(captured.is_empty(), "no request expected, got {captured:?}");
    }
}
