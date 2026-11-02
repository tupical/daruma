//! `daruma_complete` wire contract for the `comment` alias.
//!
//! Older shipped next-prompts told agents to close with
//! `daruma_complete … comment="<summary>"`; the handler used to drop that
//! argument, so the work summary was silently lost. The stub records every
//! request, pinning: `comment` (string or `{body, kind?}`) lands in
//! `note.result_summary` and, after the completion is accepted, as a task
//! comment; an identical explicit `result_summary` is fine, a different one is
//! rejected before any request, a rejected completion posts no comment, and a
//! bare `{id}` stays the legacy command without a `note`.

use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, http::StatusCode, routing::any, Router};
use daruma_mcp::tools::call_tool;
use daruma_mcp::ApiClient;
use serde_json::{json, Value};

/// `(path, body)` of every write; `/v1/commands` bodies are unwrapped to
/// the command. `complete_ok = false` answers the command with a 409.
async fn with_recording_stub(
    args: Value,
    complete_ok: bool,
) -> (anyhow::Result<Value>, Vec<(String, Value)>) {
    let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let router = Router::new().fallback(any(move |req: Request<Body>| {
        let sink = sink.clone();
        async move {
            let path = req.uri().path().to_string();
            if req.method() == axum::http::Method::GET {
                // The post-call task-state read is not part of the contract.
                return (StatusCode::OK, axum::Json(json!({"status": "done"})));
            }
            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            if path == "/v1/commands" {
                sink.lock().unwrap().push((path, body["command"].clone()));
                if !complete_ok {
                    let resp = json!({"error": {"code": "conflict", "message": "task_blocked"}});
                    return (StatusCode::CONFLICT, axum::Json(resp));
                }
                (
                    StatusCode::OK,
                    axum::Json(json!({"success": true, "data": []})),
                )
            } else {
                sink.lock().unwrap().push((path, body.clone()));
                let resp = json!({"id": "cmt_1", "body": body["body"], "kind": body["kind"]});
                (StatusCode::CREATED, axum::Json(resp))
            }
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
async fn string_comment_is_stored_as_result_summary_and_task_comment() {
    let (result, seen) = with_recording_stub(
        json!({"task_id": "tsk_1", "comment": "shipped the fix"}),
        true,
    )
    .await;
    let result = result.expect("call succeeds");
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen[0].0, "/v1/commands");
    assert_eq!(seen[0].1["type"], "complete_task");
    assert_eq!(seen[0].1["id"], "tsk_1");
    assert_eq!(
        seen[0].1["note"],
        json!({"result_summary": "shipped the fix"})
    );
    assert_eq!(seen[1].0, "/v1/tasks/tsk_1/comments");
    assert_eq!(
        seen[1].1,
        json!({"body": "shipped the fix", "kind": "outcome"})
    );
    assert_eq!(result["comment"]["id"], "cmt_1", "{result}");
}

#[tokio::test]
async fn object_comment_body_and_kind_are_kept_beside_other_note_fields() {
    let (result, seen) = with_recording_stub(
        json!({
            "id": "tsk_1",
            "reason": "accepted",
            "comment": {"body": "summary text", "kind": "Progress"}
        }),
        true,
    )
    .await;
    result.expect("call succeeds");
    assert_eq!(
        seen[0].1["note"],
        json!({"reason": "accepted", "result_summary": "summary text"})
    );
    assert_eq!(
        seen[1].1,
        json!({"body": "summary text", "kind": "progress"})
    );
}

#[tokio::test]
async fn identical_result_summary_and_comment_are_one_summary() {
    let (result, seen) = with_recording_stub(
        json!({"id": "tsk_1", "result_summary": "same", "comment": "same"}),
        true,
    )
    .await;
    result.expect("call succeeds");
    assert_eq!(seen[0].1["note"], json!({"result_summary": "same"}));
    assert_eq!(seen[1].1["body"], "same");
}

#[tokio::test]
async fn blank_comment_and_bare_id_keep_the_legacy_command() {
    for args in [
        json!({"id": "tsk_1"}),
        json!({"id": "tsk_1", "comment": "   "}),
    ] {
        let (result, seen) = with_recording_stub(args.clone(), true).await;
        result.expect("call succeeds");
        assert_eq!(seen.len(), 1, "{args}: {seen:?}");
        assert_eq!(
            seen[0].1,
            json!({"type": "complete_task", "id": "tsk_1"}),
            "{args}"
        );
    }
}

#[tokio::test]
async fn rejected_completion_posts_no_comment() {
    let (result, seen) =
        with_recording_stub(json!({"id": "tsk_1", "comment": "summary"}), false).await;
    assert!(result.is_err(), "{result:?}");
    assert_eq!(seen.len(), 1, "only the command: {seen:?}");
    assert_eq!(seen[0].0, "/v1/commands");
}

#[tokio::test]
async fn conflicting_or_malformed_comment_is_rejected_before_any_request() {
    for args in [
        json!({"id": "tsk_1", "result_summary": "one", "comment": "two"}),
        json!({"id": "tsk_1", "comment": {"kind": "outcome"}}),
        json!({"id": "tsk_1", "comment": {"body": "x", "kind": "bogus"}}),
        json!({"id": "tsk_1", "comment": 42}),
    ] {
        let (result, seen) = with_recording_stub(args.clone(), true).await;
        assert!(result.is_err(), "{args}");
        assert!(seen.is_empty(), "no request must be sent for {args}");
    }
}
