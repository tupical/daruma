//! End-to-end integration test for the Comments HTTP API.
//!
//! AC-1: create task → post comment → get comments → patch comment →
//!       delete comment → verify event log contains all expected events.
//!
//! After Wave 2 / W2.2, every `/v1/*` endpoint sits behind the bearer
//! auth middleware — so the test harness has to mint an admin token and
//! attach `Authorization: Bearer ...` to every request.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::Value;
use tower::ServiceExt;

mod common;
use common::test_app;

// ── request helpers (every request carries the bearer token) ──────────────────

async fn post_json(app: axum::Router, token: &str, uri: &str, body: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn get_json(app: axum::Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn patch_json(app: axum::Router, token: &str, uri: &str, body: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::PATCH)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn delete_req(app: axum::Router, token: &str, uri: &str) -> StatusCode {
    let req = Request::builder()
        .method(Method::DELETE)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

// ── AC-1: full comment lifecycle ───────────────────────────────────────────────

#[tokio::test]
async fn ac1_comment_lifecycle() {
    let h = test_app().await;
    let app = h.router.clone();
    let token = h.admin_token.clone();
    let store = h.event_store();

    // 1. Create a task via /v1/commands to obtain a real task_id.
    let (status, events_json) = post_json(
        app.clone(),
        &token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"AC-1 task"}}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "CreateTask should return 200");

    let task_id = events_json["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == "task_created" {
                p.get("task")?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("task_created event must contain task.id");

    // 2. POST /v1/tasks/{task_id}/comments → 201 + comment body.
    let (status, comment_json) = post_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/comments"),
        r#"{"body":"First comment"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "POST comment should return 201"
    );
    assert_eq!(comment_json["body"], "First comment");
    let comment_id = comment_json["id"]
        .as_str()
        .expect("comment must have an id")
        .to_owned();

    // 3. GET /v1/tasks/{task_id}/comments → list with the one comment.
    let (status, list_json) = get_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/comments"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "GET comments should return 200");
    let arr = list_json.as_array().unwrap();
    assert_eq!(arr.len(), 1, "should have exactly one comment");
    assert_eq!(arr[0]["id"].as_str().unwrap(), comment_id);
    assert_eq!(arr[0]["body"], "First comment");

    // 4. PATCH /v1/comments/{comment_id} → 200 + updated body.
    let (status, updated_json) = patch_json(
        app.clone(),
        &token,
        &format!("/v1/comments/{comment_id}"),
        r#"{"body":"Edited comment"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "PATCH comment should return 200");
    assert_eq!(updated_json["body"], "Edited comment");

    // 5. DELETE /v1/comments/{comment_id} → 204.
    let status = delete_req(app.clone(), &token, &format!("/v1/comments/{comment_id}")).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "DELETE comment should return 204"
    );

    // 6. GET after delete → empty list (soft-delete excluded from projection).
    let (status, list_after) = get_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/comments"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        list_after.as_array().unwrap().len(),
        0,
        "deleted comment must not appear in list"
    );

    // 7. Verify event log contains all four expected event kinds.
    let all_events = store.load_since(0, 100).await.unwrap();
    let kinds: Vec<&str> = all_events.iter().map(|e| e.kind()).collect();
    assert!(
        kinds.contains(&"task_created"),
        "event log must contain task_created"
    );
    assert!(
        kinds.contains(&"comment_added"),
        "event log must contain comment_added"
    );
    assert!(
        kinds.contains(&"comment_edited"),
        "event log must contain comment_edited"
    );
    assert!(
        kinds.contains(&"comment_deleted"),
        "event log must contain comment_deleted"
    );
}

// ── §3.8.8: optional Comment.kind ─────────────────────────────────────────────

#[tokio::test]
async fn comment_kind_default_is_none_and_research_round_trips() {
    let h = test_app().await;
    let app = h.router.clone();
    let token = h.admin_token.clone();

    // Create a task.
    let (_, events_json) = post_json(
        app.clone(),
        &token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"kind task"}}}"#,
    )
    .await;
    let task_id = events_json["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == "task_created" {
                p.get("task")?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("task_created event must contain task.id");

    // (a) POST without kind → 201, response omits kind (None).
    let (status, c1) = post_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/comments"),
        r#"{"body":"no kind"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        c1.get("kind").is_none(),
        "kind should be omitted when None, got: {c1}"
    );

    // (b) POST with kind="Research" (PascalCase, matching task spec) →
    //     201, response carries `kind: "research"`.
    let (status, c2) = post_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/comments"),
        r#"{"body":"saw paper X","kind":"Research"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        c2.get("kind").and_then(|v| v.as_str()),
        Some("research"),
        "kind must round-trip canonical snake_case, got: {c2}"
    );

    // (c) POST with kind="bogus" → 400 validation error.
    let (status, _err) = post_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/comments"),
        r#"{"body":"bad","kind":"bogus"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "unknown kind must yield HTTP 400"
    );

    // (d) Listing the task's comments shows kind on the research entry and
    //     no kind on the unclassified one.
    let (status, list_json) = get_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/comments"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let arr = list_json.as_array().unwrap();
    assert_eq!(
        arr.len(),
        2,
        "two successful comments expected, got: {arr:?}"
    );
    let kinds: Vec<Option<&str>> = arr
        .iter()
        .map(|c| c.get("kind").and_then(|v| v.as_str()))
        .collect();
    assert!(
        kinds.contains(&None),
        "one comment must have kind=None, got: {kinds:?}"
    );
    assert!(
        kinds.contains(&Some("research")),
        "one comment must have kind=research, got: {kinds:?}"
    );
}
