//! `daruma_complete` → `complete_task` wire contract for the `comment` alias.
//!
//! Older shipped next-prompts told agents to close with
//! `daruma_complete … comment="<summary>"`; the handler used to drop that
//! argument, so the work summary was silently lost. The stub records the
//! exact command body, pinning: `comment` (string or `{body}`) lands in
//! `note.result_summary`, an identical explicit `result_summary` is fine, a
//! different one is rejected before any request, and a bare `{id}` stays the
//! legacy command without a `note`.

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
                "complete must go through the command bus"
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
    let result = call_tool(&client, "daruma_complete", args).await;
    let recorded = seen.lock().unwrap().clone();
    (result, recorded)
}

#[tokio::test]
async fn string_comment_is_stored_as_result_summary() {
    let (result, seen) = with_recording_stub(json!({
        "task_id": "tsk_1", "comment": "shipped the fix"
    }))
    .await;
    result.expect("call succeeds");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["type"], "complete_task");
    assert_eq!(seen[0]["id"], "tsk_1");
    assert_eq!(
        seen[0]["note"],
        json!({"result_summary": "shipped the fix"})
    );
}

#[tokio::test]
async fn object_comment_body_is_stored_beside_other_note_fields() {
    let (result, seen) = with_recording_stub(json!({
        "id": "tsk_1",
        "reason": "accepted",
        "comment": {"body": "summary text", "kind": "outcome"}
    }))
    .await;
    result.expect("call succeeds");
    assert_eq!(
        seen[0]["note"],
        json!({"reason": "accepted", "result_summary": "summary text"})
    );
}

#[tokio::test]
async fn identical_result_summary_and_comment_are_one_summary() {
    let (result, seen) = with_recording_stub(json!({
        "id": "tsk_1", "result_summary": "same", "comment": "same"
    }))
    .await;
    result.expect("call succeeds");
    assert_eq!(seen[0]["note"], json!({"result_summary": "same"}));
}

#[tokio::test]
async fn blank_comment_and_bare_id_keep_the_legacy_command() {
    for args in [
        json!({"id": "tsk_1"}),
        json!({"id": "tsk_1", "comment": "   "}),
    ] {
        let (result, seen) = with_recording_stub(args.clone()).await;
        result.expect("call succeeds");
        assert_eq!(
            seen[0],
            json!({"type": "complete_task", "id": "tsk_1"}),
            "{args}"
        );
    }
}

#[tokio::test]
async fn conflicting_or_malformed_comment_is_rejected_before_any_request() {
    for args in [
        json!({"id": "tsk_1", "result_summary": "one", "comment": "two"}),
        json!({"id": "tsk_1", "comment": {"kind": "outcome"}}),
        json!({"id": "tsk_1", "comment": 42}),
    ] {
        let (result, seen) = with_recording_stub(args.clone()).await;
        assert!(result.is_err(), "{args}");
        assert!(seen.is_empty(), "no request must be sent for {args}");
    }
}
