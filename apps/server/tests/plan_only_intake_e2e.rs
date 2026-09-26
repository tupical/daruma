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
    let app = TestAppBuilder::default()
        .plan_only_intake(true)
        .build()
        .await;

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
    let app = TestAppBuilder::default()
        .plan_only_intake(true)
        .build()
        .await;

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
            "plan": { "title": "POI plan", "project_id": project_id, "owner": { "kind": "user" }, "source_brief": "raw prompt" },
            "tasks": [ { "title": "POI task" } ]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["success"], true, "{body}");
    let envelopes = body["data"]
        .as_array()
        .unwrap_or_else(|| panic!("materialize must return event envelopes: {body}"));
    let types: Vec<&str> = envelopes
        .iter()
        .filter_map(|e| e["payload"]["type"].as_str())
        .collect();
    assert!(types.contains(&"plan_created"), "{types:?}");
    assert!(types.contains(&"task_created"), "{types:?}");
    assert!(types.contains(&"plan_task_added"), "{types:?}");
    let created = envelopes
        .iter()
        .find(|e| e["payload"]["type"] == "plan_created")
        .unwrap_or_else(|| panic!("no plan_created envelope: {body}"));
    assert_eq!(created["payload"]["plan"]["source_brief"], "raw prompt");
}

/// ADR-0009 over HTTP: the `intake_source` policy is written through
/// PATCH /settings (invalid → 422), read back by GET, and an `enforce`
/// policy turns a source-less materialize into 422 `plan_source_required`
/// listing the project's channels.
#[tokio::test]
async fn intake_source_policy_enforced_over_http() {
    use axum::body::{to_bytes, Body};
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    let app = TestAppBuilder::default()
        .plan_only_intake(true)
        .build()
        .await;
    let (_, body) = post_command(&app, json!({ "type": "create_project", "title": "Src" })).await;
    let project_id = body["data"][0]["payload"]["project"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let patch = |policy: serde_json::Value| {
        let router = app.router.clone();
        let req = Request::builder()
            .method(Method::PATCH)
            .uri(format!("/v1/projects/{project_id}/settings"))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", app.admin_token))
            .body(Body::from(json!({ "intake_source": policy }).to_string()))
            .unwrap();
        async move {
            let res = router.oneshot(req).await.unwrap();
            let status = res.status();
            let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default(),
            )
        }
    };

    let (status, body) =
        patch(json!({ "mode": "enforce", "channels": [{ "scheme": "https", "pattern": "(" }] }))
            .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "invalid_intake_source", "{body}");
    let (status, _) = patch(json!({ "mode": "strict" })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let channel = json!({ "scheme": "https", "label": "Issue" });
    let (status, body) = patch(json!({ "mode": "enforce", "channels": [channel] })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["intake_source"]["mode"], "enforce", "{body}");
    let (_, body) = common::json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/projects/{project_id}/settings"),
    )
    .await;
    assert_eq!(
        body["intake_source"]["channels"][0]["label"], "Issue",
        "{body}"
    );

    let materialize = |plan: serde_json::Value| {
        post_command(
            &app,
            json!({ "type": "materialize_plan", "plan": plan, "tasks": [{ "title": "t" }] }),
        )
    };
    let (status, body) =
        materialize(json!({ "title": "p", "project_id": project_id, "owner": { "kind": "user" } }))
            .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "plan_source_required", "{body}");
    assert_eq!(body["error"]["reason"], "plan_source_missing", "{body}");
    assert_eq!(body["error"]["channels"][0]["label"], "Issue", "{body}");

    let (status, body) = materialize(json!({
        "title": "p", "project_id": project_id, "owner": { "kind": "user" },
        "source_ref": "https://gitlab.x/g/p/-/issues/1"
    }))
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = patch(serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["data"]["intake_source"].is_null(), "{body}");
}

/// A stored policy this server cannot read (e.g. a newer `mode`) must not
/// block intake: it degrades to `warn` without channels.
#[tokio::test]
async fn unreadable_intake_source_policy_degrades_to_warn() {
    let app = TestAppBuilder::default()
        .plan_only_intake(true)
        .build()
        .await;
    let (_, body) =
        post_command(&app, json!({ "type": "create_project", "title": "Future" })).await;
    let project_id = body["data"][0]["payload"]["project"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    sqlx::query(
        "INSERT INTO project_settings (project_id, key, value, updated_at) \
         VALUES (?, 'intake_source', '{\"mode\":\"strict_v2\"}', '2026-09-25T00:00:00Z')",
    )
    // Rows are keyed by the prefixed display form (`prj_…`), not the wire form.
    .bind(
        project_id
            .parse::<daruma_shared::ProjectId>()
            .unwrap()
            .to_string(),
    )
    .execute(&app.pool)
    .await
    .unwrap();
    // The row must really be unreadable, or this test proves nothing.
    assert!(app
        .state
        .project_settings
        .intake_source(project_id.parse().unwrap())
        .await
        .is_err());

    let (status, body) = post_command(
        &app,
        json!({
            "type": "materialize_plan",
            "plan": { "title": "p", "project_id": project_id, "owner": { "kind": "user" } },
            "tasks": [{ "title": "t" }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["warnings"][0]["code"], "plan_source_missing", "{body}");

    let (status, body) = common::json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/projects/{project_id}/settings"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["intake_source"].is_null(), "{body}");
    assert!(body["intake_source_error"].is_string(), "{body}");
}

/// `POST /v1/plans` (plan_create) follows the same source policy as
/// materialize: 422 in enforce without a source, warnings in warn.
#[tokio::test]
async fn create_plan_over_http_follows_source_policy() {
    use axum::body::{to_bytes, Body};
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    let app = TestAppBuilder::default()
        .plan_only_intake(true)
        .build()
        .await;
    let (_, body) = post_command(&app, json!({ "type": "create_project", "title": "Plans" })).await;
    let project_id = body["data"][0]["payload"]["project"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let send = |method: Method, uri: String, body: serde_json::Value| {
        let router = app.router.clone();
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", app.admin_token))
            .body(Body::from(body.to_string()))
            .unwrap();
        async move {
            let res = router.oneshot(req).await.unwrap();
            let status = res.status();
            let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default(),
            )
        }
    };
    let create = |extra: serde_json::Value| {
        let mut plan =
            json!({ "title": "p", "project_id": project_id, "owner": { "kind": "user" } });
        for (k, v) in extra.as_object().unwrap() {
            plan[k] = v.clone();
        }
        send(Method::POST, "/v1/plans".into(), json!({ "plan": plan }))
    };

    // Default (no policy) = warn: created, warning in the response.
    let (status, body) = create(json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["warnings"][0]["code"], "plan_source_missing", "{body}");

    let (status, body) = send(
        Method::PATCH,
        format!("/v1/projects/{project_id}/settings"),
        json!({ "intake_source": { "mode": "enforce" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = create(json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "plan_source_required", "{body}");

    let (status, body) = create(json!({ "source_ref": "https://gitlab.x/g/p/-/issues/1" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body.get("warnings").is_none(), "{body}");
}
