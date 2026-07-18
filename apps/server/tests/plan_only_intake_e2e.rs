//! ADR-0007 plan-only intake — HTTP-transport bridge tests.
//!
//! The production server always runs with `plan_only_intake` on
//! (`apps/server/src/main.rs`); this exercises the same flag through the
//! `POST /v1/commands` surface: `create_task` must be rejected with the
//! structured bridge error naming the replacement, while `materialize_plan`
//! stays the sole intake path. There is no separate `POST /tasks` route —
//! `/v1/commands` is the only HTTP create surface.

use axum::http::StatusCode;
use serde_json::json;

mod common;
use common::{json_post, TestAppBuilder};

/// POST one command envelope and return (status, body).
async fn post_command(
    app: &common::TestApp,
    command: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let body = json!({ "command": command, "actor": { "kind": "user" } }).to_string();
    json_post(app.router.clone(), &app.admin_token, "/v1/commands", &body).await
}

#[tokio::test]
async fn create_task_bridged_when_plan_only_intake_on() {
    let app = TestAppBuilder::default().plan_only_intake(true).build().await;

    let (status, body) = post_command(
        &app,
        json!({ "type": "create_task", "task": { "title": "direct create" } }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("plan_only_intake"), "{message}");
    assert!(
        message.contains("MaterializePlan"),
        "bridge must name the replacement: {message}"
    );
}

#[tokio::test]
async fn create_task_allowed_when_plan_only_intake_off() {
    // Control case: without the flag the legacy path still serves the
    // desktop offline executor and in-process callers (ADR-0007 clarification).
    let app = TestAppBuilder::default().build().await;

    let (status, body) = post_command(
        &app,
        json!({ "type": "create_task", "task": { "title": "legacy create" } }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["success"], true, "{body}");
}

#[tokio::test]
async fn materialize_plan_accepted_when_plan_only_intake_on() {
    let app = TestAppBuilder::default().plan_only_intake(true).build().await;

    let (status, body) = post_command(
        &app,
        json!({ "type": "create_project", "title": "POI project" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let project_id = body["data"][0]["payload"]["project"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no project id in {body}"))
        .to_owned();

    let (status, body) = post_command(
        &app,
        json!({
            "type": "materialize_plan",
            "plan": { "title": "POI plan", "project_id": project_id, "owner": { "kind": "user" } },
            "tasks": [ { "title": "POI task" } ]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["success"], true, "{body}");
    let envelopes = body["data"].as_array().unwrap_or_else(|| {
        panic!("materialize must return event envelopes: {body}")
    });
    let types: Vec<&str> = envelopes
        .iter()
        .filter_map(|e| e["payload"]["type"].as_str())
        .collect();
    assert!(types.contains(&"plan_created"), "{types:?}");
    assert!(types.contains(&"task_created"), "{types:?}");
    assert!(types.contains(&"plan_task_added"), "{types:?}");
}
