//! `daruma_set_status` → `set_status` wire contract for the fused `comment`
//! argument (Action Fusion of `daruma_comment` + `daruma_set_status`).
//!
//! The stub records the exact command body the MCP layer posts, so the test
//! pins: `comment` is forwarded as `{body, kind}` with the kind normalised to
//! snake_case, omitted entirely when not given, and rejected client-side
//! when `body` is missing or the kind is unknown (no HTTP round-trip).

use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, http::StatusCode, routing::any, Router};
use daruma_mcp::tools::call_tool;
use daruma_mcp::ApiClient;
use serde_json::{json, Value};

async fn with_recording_stub(args: Value) -> (anyhow::Result<Value>, Vec<Value>) {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let router = Router::new().fallback(any(move |req: Request<Body>| {
        let sink = sink.clone();
        async move {
            let path = req.uri().path().to_string();
            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            assert_eq!(
                path, "/v1/commands",
                "set_status must go through the command bus"
            );
            sink.lock().unwrap().push(body["command"].clone());
            let resp = json!({"success": true, "data": []});
            (StatusCode::OK, axum::Json(resp))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = ApiClient::new(format!("http://{addr}"), "test-token");
    let result = call_tool(&client, "daruma_set_status", args).await;
    let recorded = seen.lock().unwrap().clone();
    (result, recorded)
}

#[tokio::test]
async fn comment_is_forwarded_with_normalised_kind() {
    let (result, seen) = with_recording_stub(json!({
        "id": "tsk_1", "status": "in_progress",
        "comment": {"body": "starting", "kind": "Intent"}
    }))
    .await;
    result.expect("call succeeds");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["type"], "set_status");
    assert_eq!(
        seen[0]["comment"],
        json!({"body": "starting", "kind": "intent"})
    );
}

#[tokio::test]
async fn comment_without_kind_and_absent_comment() {
    let (_, seen) = with_recording_stub(json!({
        "id": "tsk_1", "status": "done", "comment": {"body": "shipped"}
    }))
    .await;
    assert_eq!(seen[0]["comment"], json!({"body": "shipped"}));

    let (_, seen) = with_recording_stub(json!({"id": "tsk_1", "status": "done"})).await;
    assert!(
        seen[0].get("comment").is_none(),
        "legacy call must not send a comment key: {}",
        seen[0]
    );
}

#[tokio::test]
async fn malformed_comment_is_rejected_before_any_request() {
    for args in [
        json!({"id": "tsk_1", "status": "done", "comment": {"kind": "outcome"}}),
        json!({"id": "tsk_1", "status": "done", "comment": {"body": "x", "kind": "vibes"}}),
    ] {
        let (result, seen) = with_recording_stub(args.clone()).await;
        assert!(result.is_err(), "{args}");
        assert!(seen.is_empty(), "no request must be sent for {args}");
    }
}
