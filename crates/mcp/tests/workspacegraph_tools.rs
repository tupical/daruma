//! MCP shim coverage for WorkspaceGraph read tools.

use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, http::StatusCode, routing::any, Router};
use daruma_mcp::tools::call_tool;
use daruma_mcp::{tool_definitions, ApiClient};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
struct Captured {
    path: String,
}

fn recording_router(capture: Arc<Mutex<Option<Captured>>>) -> Router {
    Router::new().fallback(any(move |req: Request<Body>| {
        let capture = capture.clone();
        async move {
            *capture.lock().unwrap() = Some(Captured {
                path: req.uri().path().to_string(),
            });
            (StatusCode::OK, axum::Json(json!({})))
        }
    }))
}

async fn call_via_mock(tool: &str, args: Value) -> Captured {
    let captured: Arc<Mutex<Option<Captured>>> = Arc::new(Mutex::new(None));
    let router = recording_router(captured.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let client = ApiClient::new(format!("http://{addr}"), "test-token");
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
fn catalogue_contains_workspacegraph_tools() {
    let names = tool_definitions()
        .iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    for expected in [
        "daruma_workspacegraph_status",
        "daruma_workspacegraph_context",
        "daruma_workspacegraph_related",
        "daruma_workspacegraph_search",
        "daruma_workspacegraph_impact",
    ] {
        assert!(
            names.contains(&expected),
            "tool_definitions() is missing {expected}; found: {names:?}"
        );
    }
}

#[tokio::test]
async fn workspacegraph_status_gets_status_endpoint() {
    let captured = call_via_mock("daruma_workspacegraph_status", json!({})).await;
    assert_eq!(captured.path, "/v1/workspacegraph/status");
}

#[tokio::test]
async fn workspacegraph_context_gets_context_endpoint() {
    let captured = call_via_mock(
        "daruma_workspacegraph_context",
        json!({"node_id": "task:tsk_1", "limit": 10}),
    )
    .await;
    assert_eq!(captured.path, "/v1/workspacegraph/context");
}

#[tokio::test]
async fn workspacegraph_related_gets_related_endpoint() {
    let captured = call_via_mock(
        "daruma_workspacegraph_related",
        json!({"node_id": "task:tsk_1", "depth": 2, "limit": 15}),
    )
    .await;
    assert_eq!(captured.path, "/v1/workspacegraph/related");
}

#[tokio::test]
async fn workspacegraph_search_gets_search_endpoint() {
    let captured = call_via_mock(
        "daruma_workspacegraph_search",
        json!({"query": "needle", "project_id": "all", "limit": 5}),
    )
    .await;
    assert_eq!(captured.path, "/v1/workspacegraph/search");
}

#[tokio::test]
async fn workspacegraph_impact_gets_impact_endpoint() {
    let captured = call_via_mock(
        "daruma_workspacegraph_impact",
        json!({"node_id": "task:tsk_1", "limit": 8}),
    )
    .await;
    assert_eq!(captured.path, "/v1/workspacegraph/impact");
}
