mod common;

use axum::http::StatusCode;
use axum::{
    body::{to_bytes, Body},
    http::{Method, Request},
};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn create_project(app: &common::TestApp) -> String {
    let (status, body) = common::json_post(
        app.router.clone(),
        &app.admin_token,
        "/v1/commands",
        r#"{"command":{"type":"create_project","title":"Triage Project"}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create_project failed: {body}");
    extract_event_id(&body, "project_created", "project")
}

fn extract_event_id(body: &Value, event_type: &str, entity: &str) -> String {
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|event| {
            let payload = event.get("payload")?;
            (payload.get("type")?.as_str()? == event_type)
                .then(|| payload.get(entity)?.get("id")?.as_str().map(str::to_owned))
                .flatten()
        })
        .expect("event id")
}

async fn json_patch(app: axum::Router, token: &str, uri: &str, body: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::PATCH)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn triage_queue_lists_pending_tasks_and_accepts_them() {
    let app = common::test_app().await;
    let project_id = create_project(&app).await;

    let (status, project) = json_patch(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/projects/{project_id}/triage"),
        r#"{"triage_enabled":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(project["triage_enabled"], true);

    let create_task = json!({
        "command": {
            "type": "create_task",
            "task": {
                "project_id": project_id,
                "title": "Needs triage",
                "triage_state": "pending"
            }
        }
    })
    .to_string();
    let (status, body) = common::json_post(
        app.router.clone(),
        &app.admin_token,
        "/v1/commands",
        &create_task,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create_task failed: {body}");
    let task_id = extract_event_id(&body, "task_created", "task");

    let (status, queue) = common::json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/projects/{project_id}/triage"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(queue.as_array().unwrap().len(), 1);
    assert_eq!(queue[0]["id"], task_id);

    let (status, task) = json_patch(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/tasks/{task_id}/triage"),
        r#"{"triage_state":"accepted"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(task["triage_state"], "accepted");

    let (status, queue) = common::json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/projects/{project_id}/triage"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(queue.as_array().unwrap().is_empty());
}
