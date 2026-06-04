mod common;

use axum::http::StatusCode;
use serde_json::json;

async fn create_project(app: &common::TestApp) -> String {
    let (status, body) = common::json_post(
        app.router.clone(),
        &app.admin_token,
        "/v1/commands",
        r#"{"command":{"type":"create_project","title":"Quota Project"}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create_project failed: {body}");
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|event| {
            let payload = event.get("payload")?;
            (payload.get("type")?.as_str()? == "project_created")
                .then(|| {
                    payload
                        .get("project")?
                        .get("id")?
                        .as_str()
                        .map(str::to_owned)
                })
                .flatten()
        })
        .expect("project_created event")
}

#[tokio::test]
async fn create_task_returns_402_when_tenant_task_quota_is_full() {
    let app = common::test_app().await;
    app.state
        .tenant_quotas
        .set_limits("self-hosted", Some(1), None, None)
        .await
        .unwrap();

    let first = r#"{"command":{"type":"create_task","task":{"title":"First"}}}"#;
    let second = r#"{"command":{"type":"create_task","task":{"title":"Second"}}}"#;

    let (status, body) =
        common::json_post(app.router.clone(), &app.admin_token, "/v1/commands", first).await;
    assert_eq!(status, StatusCode::OK, "first create_task failed: {body}");

    let (status, body) =
        common::json_post(app.router.clone(), &app.admin_token, "/v1/commands", second).await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(body["error"]["code"], "quota_exceeded");
    assert_eq!(body["error"]["resource"], "tasks");
    assert_eq!(body["error"]["limit"], 1);
    assert_eq!(body["error"]["current"], 1);
}

#[tokio::test]
async fn create_plan_returns_402_when_tenant_plan_quota_is_full() {
    let app = common::test_app().await;
    let project_id = create_project(&app).await;
    app.state
        .tenant_quotas
        .set_limits("self-hosted", None, Some(1), None)
        .await
        .unwrap();

    let first = json!({
        "command": {
            "type": "create_plan",
            "plan": {
                "project_id": project_id,
                "title": "First plan",
                "owner": { "kind": "user" }
            }
        }
    })
    .to_string();
    let second = json!({
        "command": {
            "type": "create_plan",
            "plan": {
                "project_id": project_id,
                "title": "Second plan",
                "owner": { "kind": "user" }
            }
        }
    })
    .to_string();

    let (status, body) =
        common::json_post(app.router.clone(), &app.admin_token, "/v1/commands", &first).await;
    assert_eq!(status, StatusCode::OK, "first create_plan failed: {body}");

    let (status, body) = common::json_post(
        app.router.clone(),
        &app.admin_token,
        "/v1/commands",
        &second,
    )
    .await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(body["error"]["code"], "quota_exceeded");
    assert_eq!(body["error"]["resource"], "plans");
    assert_eq!(body["error"]["limit"], 1);
    assert_eq!(body["error"]["current"], 1);
}
