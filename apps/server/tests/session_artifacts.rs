use axum::http::StatusCode;
use serde_json::json;

mod common;
use common::{json_get, json_post, test_app};

#[tokio::test]
async fn session_artifact_attach_and_list_round_trip() {
    let app = test_app().await;

    let (status, start) = json_post(
        app.router.clone(),
        &app.admin_token,
        "/v1/sessions",
        &json!({
            "agent_id": app.admin_agent_id
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "start response: {start}");
    let session_id = start["data"]["id"]
        .as_str()
        .expect("session id must be present");

    let (status, artifact) = json_post(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/sessions/{session_id}/artifacts"),
        r#"{"kind":"file","ref":"target/report.txt","metadata":{"bytes":42}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "attach response: {artifact}");
    assert_eq!(artifact["session_id"], json!(session_id));
    assert_eq!(artifact["kind"], json!("file"));
    assert_eq!(artifact["ref"], json!("target/report.txt"));
    assert_eq!(artifact["metadata"]["bytes"], json!(42));

    let (status, listed) = json_get(
        app.router,
        &app.admin_token,
        &format!("/v1/sessions/{session_id}/artifacts"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list response: {listed}");
    assert_eq!(listed["artifacts"].as_array().unwrap().len(), 1);
    assert_eq!(listed["artifacts"][0]["ref"], json!("target/report.txt"));
}
