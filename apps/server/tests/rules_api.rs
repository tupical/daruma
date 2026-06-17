//! HTTP API tests for lifecycle rules (docs/LIFECYCLE_RULES_SPEC.md §4):
//! create / get / list / patch / disable round-trip through `/v1/rules`.

mod common;

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
    Router,
};
use common::{json_get, json_post, TestAppBuilder};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn json_method(
    app: Router,
    method: Method,
    token: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_owned()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn rule_crud_roundtrip() {
    let app = TestAppBuilder::default().build().await;
    let token = app.admin_token.clone();

    // Create (example 3: completion-note at tenant scope).
    let body = json!({
        "rule": {
            "rule_key": "completion-note",
            "title": "Require who/when/why on completion",
            "scope": { "kind": "tenant" },
            "trigger": "task.before_complete",
            "requirement": {
                "type": "completion_note",
                "required_fields": ["actor", "completed_at", "reason"]
            },
            "mode": "required",
            "message": "Задачу нельзя завершить без отметки кто/когда/зачем.",
            "override_allowed": true
        }
    })
    .to_string();
    let (status, created) = json_post(app.router.clone(), &token, "/v1/rules", &body).await;
    assert_eq!(status, StatusCode::OK, "create: {created}");
    assert_eq!(created["success"], json!(true));
    let rule_id = created["data"]["rule"]["id"]
        .as_str()
        .expect("created rule id")
        .to_string();
    assert_eq!(created["data"]["rule"]["mode"], json!("required"));

    // Get by id.
    let (status, got) = json_get(app.router.clone(), &token, &format!("/v1/rules/{rule_id}")).await;
    assert_eq!(status, StatusCode::OK, "get: {got}");
    assert_eq!(got["rule"]["rule_key"], json!("completion-note"));

    // List at tenant scope.
    let (status, list) = json_get(app.router.clone(), &token, "/v1/rules").await;
    assert_eq!(status, StatusCode::OK, "list: {list}");
    assert_eq!(list["rules"].as_array().unwrap().len(), 1);

    // Duplicate rule_key at the same scope is rejected.
    let (status, _) = json_post(app.router.clone(), &token, "/v1/rules", &body).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "duplicate rule_key at one scope must conflict"
    );

    // Patch: weaken to recommendation.
    let patch = json!({ "mode": "recommendation" }).to_string();
    let (status, patched) = json_method(
        app.router.clone(),
        Method::PATCH,
        &token,
        &format!("/v1/rules/{rule_id}"),
        Some(&patch),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch: {patched}");
    assert_eq!(patched["data"]["rule"]["mode"], json!("recommendation"));

    // Disable (DELETE).
    let (status, disabled) = json_method(
        app.router.clone(),
        Method::DELETE,
        &token,
        &format!("/v1/rules/{rule_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "disable: {disabled}");

    let (_, got) = json_get(app.router.clone(), &token, &format!("/v1/rules/{rule_id}")).await;
    assert_eq!(
        got["rule"]["enabled"],
        json!(false),
        "disabled after DELETE"
    );
}

#[tokio::test]
async fn create_rule_validation_rejects_empty_key() {
    let app = TestAppBuilder::default().build().await;
    let token = app.admin_token.clone();
    let body = json!({
        "rule": {
            "rule_key": "",
            "title": "x",
            "scope": { "kind": "tenant" },
            "trigger": "task.before_complete",
            "requirement": { "type": "owner_required" },
            "mode": "required"
        }
    })
    .to_string();
    let (status, _) = json_post(app.router.clone(), &token, "/v1/rules", &body).await;
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "empty rule_key must be rejected, got {status}"
    );
}
