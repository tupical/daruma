//! MCP shim coverage for plan graph/fanout/can_start tools.

use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, http::StatusCode, routing::any, Router};
use daruma_mcp::tools::call_tool;
use daruma_mcp::{tool_definitions, ApiClient};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
struct Captured {
    path: String,
    query: Option<String>,
}

fn recording_router(capture: Arc<Mutex<Option<Captured>>>) -> Router {
    Router::new().fallback(any(move |req: Request<Body>| {
        let capture = capture.clone();
        async move {
            *capture.lock().unwrap() = Some(Captured {
                path: req.uri().path().to_string(),
                query: req.uri().query().map(str::to_string),
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
fn catalogue_contains_plan_readiness_tools() {
    let names = tool_definitions()
        .iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    for expected in [
        "daruma_plan_graph",
        "daruma_plan_fanout",
        "daruma_plan_drain_next",
        "daruma_can_start",
        "daruma_search",
        "daruma_lesson_recall",
    ] {
        assert!(
            names.contains(&expected),
            "tool_definitions() is missing {expected}; found: {names:?}"
        );
    }
}

#[tokio::test]
async fn plan_graph_posts_to_graph_endpoint() {
    let captured = call_via_mock("daruma_plan_graph", json!({"plan_id": "pln_1"})).await;
    assert_eq!(captured.path, "/v1/plans/pln_1/graph");
}

#[tokio::test]
async fn plan_fanout_posts_to_fanout_endpoint() {
    let captured = call_via_mock("daruma_plan_fanout", json!({"plan_id": "pln_1"})).await;
    assert_eq!(captured.path, "/v1/plans/pln_1/fanout");
}

#[tokio::test]
async fn plan_drain_next_posts_to_drain_endpoint() {
    let captured = call_via_mock(
        "daruma_plan_drain_next",
        json!({"plan_id": "pln_1", "claim_ttl_secs": 60}),
    )
    .await;
    assert_eq!(captured.path, "/v1/plans/pln_1/drain-next");
}

#[tokio::test]
async fn can_start_posts_to_task_endpoint() {
    let captured = call_via_mock("daruma_can_start", json!({"task_id": "tsk_1"})).await;
    assert_eq!(captured.path, "/v1/tasks/tsk_1/can_start");
}

#[tokio::test]
async fn search_posts_to_search_endpoint() {
    let captured = call_via_mock("daruma_search", json!({"query": "needle"})).await;
    assert_eq!(captured.path, "/v1/search");
}

#[tokio::test]
async fn list_forwards_limit_to_tasks_endpoint() {
    let captured = call_via_mock(
        "daruma_list",
        json!({"project_id": "all", "status": "active", "limit": 10}),
    )
    .await;
    assert_eq!(captured.path, "/v1/tasks");
    let query = captured.query.expect("query string must be present");
    assert!(
        query.contains("limit=10"),
        "daruma_list must forward limit: {query}"
    );
    assert!(
        query.contains("page=true"),
        "daruma_list must request paged responses: {query}"
    );
}

#[tokio::test]
async fn plan_list_forwards_limit_to_plans_endpoint() {
    let captured = call_via_mock(
        "daruma_plan_list",
        json!({"project_id": "pln_project", "status": "active", "limit": 10}),
    )
    .await;
    assert_eq!(captured.path, "/v1/plans");
    let query = captured.query.expect("query string must be present");
    assert!(
        query.contains("limit=10"),
        "daruma_plan_list must forward limit: {query}"
    );
    assert!(
        query.contains("page=true"),
        "daruma_plan_list must request paged responses: {query}"
    );
}

#[tokio::test]
async fn lesson_recall_searches_lesson_comment_prefix() {
    let captured = call_via_mock(
        "daruma_lesson_recall",
        json!({"query": "branch", "project_id": "all", "limit": 5}),
    )
    .await;
    assert_eq!(captured.path, "/v1/search");
    let query = captured.query.expect("query string must be present");
    assert!(
        query.contains("query=lesson%3A%20branch"),
        "wrong lesson query string: {query}"
    );
    assert!(
        query.contains("scope=comments"),
        "lesson recall must restrict scope to comments: {query}"
    );
    assert!(
        query.contains("project_id=all"),
        "lesson recall must forward project_id=all: {query}"
    );
    assert!(
        query.contains("limit=5"),
        "lesson recall must forward limit: {query}"
    );
}
