use axum::http::StatusCode;
use serde_json::json;

mod common;
use common::{json_get, json_post, test_app};

#[tokio::test]
async fn session_start_get_and_list_with_metadata() {
    let app = test_app().await;

    let metadata = json!({
        "client": "cursor",
        "model": "composer-2.5",
        "chat_id": "chat-test-1",
        "transcript_path": "/tmp/transcript.jsonl"
    });

    let (status, start) = json_post(
        app.router.clone(),
        &app.admin_token,
        "/v1/sessions",
        &json!({
            "agent_id": app.admin_agent_id,
            "metadata": metadata
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "start: {start}");
    assert_eq!(start["data"]["metadata"]["client"], "cursor");
    assert_eq!(start["data"]["metadata"]["model"], "composer-2.5");

    let session_id = start["data"]["id"].as_str().expect("session id");

    let (status, got) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/sessions/{session_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get: {got}");
    assert_eq!(got["metadata"]["chat_id"], "chat-test-1");

    let (status, listed) = json_get(
        app.router,
        &app.admin_token,
        &format!("/v1/sessions?agent_id={}", app.admin_agent_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list: {listed}");
    assert!(
        listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == session_id),
        "session should appear in list"
    );
}
