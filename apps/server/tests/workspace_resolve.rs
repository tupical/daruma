//! `/v1/workspace-registry/resolve` — path → logical workspace/project
//! context with create-or-bind for unknown roots.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::Value;
use tower::ServiceExt;

mod common;
use common::test_app;

async fn post_json(app: &axum::Router, token: &str, uri: &str, body: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn unknown_root_creates_workspace_and_project_idempotently() {
    let h = test_app().await;
    let admin = &h.admin_token;

    let body = r#"{"root_path":"/home/dev/projects/acme-api"}"#;
    let (s, first) = post_json(&h.router, admin, "/v1/workspace-registry/resolve", body).await;
    assert_eq!(s, StatusCode::OK, "{first}");
    assert_eq!(first["resolved"], true);
    assert_eq!(first["created_workspace"], true);
    assert_eq!(first["created_project"], true);
    assert_eq!(first["workspace_id"], "acme-api");
    let project_id = first["project_id"].as_str().unwrap().to_owned();

    // Same root again → same context, nothing created.
    let (s, second) = post_json(&h.router, admin, "/v1/workspace-registry/resolve", body).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(second["created_workspace"], false);
    assert_eq!(second["created_project"], false);
    assert_eq!(second["project_id"].as_str().unwrap(), project_id);

    // A subdirectory of the bound root resolves to the same project.
    let (s, sub) = post_json(
        &h.router,
        admin,
        "/v1/workspace-registry/resolve",
        r#"{"root_path":"/home/dev/projects/acme-api/crates/core"}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(sub["project_id"].as_str().unwrap(), project_id);
    assert_eq!(sub["created_project"], false);
}

#[tokio::test]
async fn probe_only_does_not_create() {
    let h = test_app().await;
    let admin = &h.admin_token;

    let (s, resp) = post_json(
        &h.router,
        admin,
        "/v1/workspace-registry/resolve",
        r#"{"root_path":"/home/dev/projects/unseen","create":false}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(resp["resolved"], false, "{resp}");

    // Still unknown afterwards — the probe must not have created anything.
    let (s, resp) = post_json(
        &h.router,
        admin,
        "/v1/workspace-registry/resolve",
        r#"{"root_path":"/home/dev/projects/unseen","create":false}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(resp["resolved"], false);
}

#[tokio::test]
async fn known_workspace_root_gets_project_bound_into_it() {
    let h = test_app().await;
    let admin = &h.admin_token;

    // Pre-create a logical workspace bound to a parent root.
    let (s, ws) = post_json(
        &h.router,
        admin,
        "/v1/workspace-registry",
        r#"{"name":"Client Work","id":"client-work","root_path":"/home/dev/clients"}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{ws}");

    // A new repo under that root lands in the existing workspace.
    let (s, resp) = post_json(
        &h.router,
        admin,
        "/v1/workspace-registry/resolve",
        r#"{"root_path":"/home/dev/clients/widgets"}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{resp}");
    assert_eq!(resp["workspace_id"], "client-work");
    assert_eq!(resp["created_workspace"], false);
    assert_eq!(resp["created_project"], true);
}

#[tokio::test]
async fn explicit_workspace_target_binds_root_there() {
    let h = test_app().await;
    let admin = &h.admin_token;

    let (s, _) = post_json(
        &h.router,
        admin,
        "/v1/workspace-registry",
        r#"{"name":"Side Projects","id":"side"}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, resp) = post_json(
        &h.router,
        admin,
        "/v1/workspace-registry/resolve",
        r#"{"root_path":"/home/dev/hack/toy","workspace_id":"side"}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{resp}");
    assert_eq!(resp["workspace_id"], "side");
    assert_eq!(resp["created_workspace"], false);
    assert_eq!(resp["created_project"], true);

    // Unknown explicit workspace → not found.
    let (s, _resp) = post_json(
        &h.router,
        admin,
        "/v1/workspace-registry/resolve",
        r#"{"root_path":"/home/dev/hack/toy2","workspace_id":"nope"}"#,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
