use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, http::StatusCode, routing::any, Router};
use daruma_mcp::tools::call_tool;
use daruma_mcp::{tool_definitions, ApiClient};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
struct Captured {
    method: String,
    path: String,
    body: Value,
}

fn recording_router(capture: Arc<Mutex<Option<Captured>>>) -> Router {
    Router::new().fallback(any(move |req: Request<Body>| {
        let capture = capture.clone();
        async move {
            let method = req.method().to_string();
            let path = req.uri().path().to_string();
            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .unwrap_or_default();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            *capture.lock().unwrap() = Some(Captured { method, path, body });
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
fn catalogue_contains_session_artifact_tools() {
    let names = tool_definitions()
        .iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"daruma_session_artifact"));
    assert!(names.contains(&"daruma_session_artifacts_list"));
}

#[tokio::test]
async fn session_artifact_posts_to_session_artifacts_endpoint() {
    let captured = call_via_mock(
        "daruma_session_artifact",
        json!({
            "session_id": "ags_123",
            "kind": "file",
            "ref": "target/report.txt",
            "metadata": {"bytes": 42}
        }),
    )
    .await;

    assert_eq!(captured.method, "POST");
    assert_eq!(captured.path, "/v1/sessions/ags_123/artifacts");
    assert_eq!(captured.body["kind"], json!("file"));
    assert_eq!(captured.body["ref"], json!("target/report.txt"));
    assert_eq!(captured.body["metadata"]["bytes"], json!(42));
}

#[tokio::test]
async fn session_artifacts_list_gets_session_artifacts_endpoint() {
    let captured = call_via_mock("daruma_session_artifacts_list", json!({"id": "ags_123"})).await;

    assert_eq!(captured.method, "GET");
    assert_eq!(captured.path, "/v1/sessions/ags_123/artifacts");
}
