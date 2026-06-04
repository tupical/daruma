//! End-to-end integration tests for GET /v1/tasks/{task_id}/activity (Section B.5 / W1.4).
//!
//! Covers:
//!   AC-2 — activity row created on `create_task`
//!   AC-3 — pair-merge: `status_changed` + `task_closed` → verb=`closed`
//!   AC-4 — two auth gates: missing `task:read` capability → 403,
//!           token scoped to wrong project → 403
//!   AC-6 — cursor pagination (limit, next_cursor, has_more)
//!   AC-7 — verb filter (?verbs=...) including unknown verb → 400

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::{json, Value};
use taskagent_shared::TaskId;
use tower::ServiceExt;

mod common;
use common::test_app;

// ── request helpers ───────────────────────────────────────────────────────────

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

// ── parse helpers ─────────────────────────────────────────────────────────────

fn extract_task_id(events: &Value) -> String {
    events["data"]
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
        .expect("expected task_created event containing task.id")
}

fn extract_project_id(events: &Value) -> String {
    events["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == "project_created" {
                p.get("project")?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("expected project_created event containing project.id")
}

// ── AC-2: activity row recorded on create_task ────────────────────────────────

#[tokio::test]
async fn creates_activity_on_task_created() {
    let h = test_app().await;
    let app = h.router.clone();
    let token = h.admin_token.clone();

    let (s, ev) = post_json(
        app.clone(),
        &token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"Activity seed"}}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let task_id = extract_task_id(&ev);

    let (s, body) = get_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/activity"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "activity endpoint must return 200");

    let items = body["items"].as_array().expect("items must be an array");
    assert!(
        !items.is_empty(),
        "must have at least one activity row after create"
    );
    assert_eq!(
        items[0]["verb"].as_str().unwrap(),
        "created",
        "first verb must be 'created'"
    );
    assert_eq!(body["has_more"], false);
}

// ── AC-3: pair-merge — StatusChanged + TaskClosed → verb = closed ─────────────

#[tokio::test]
async fn status_change_then_close_merges_into_closed_verb() {
    let h = test_app().await;
    let app = h.router.clone();
    let token = h.admin_token.clone();

    let (s, ev) = post_json(
        app.clone(),
        &token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"Pair-merge test"}}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let task_id = extract_task_id(&ev);

    // SetStatus "done" — handler emits TaskStatusChanged then TaskClosed (semantic pair).
    // TaskClosed pair-merges the StatusChanged row's verb to "closed".
    let (s, _) = post_json(
        app.clone(),
        &token,
        "/v1/commands",
        &json!({"command": {"type": "set_status", "id": task_id, "status": "done"}}).to_string(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, body) = get_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/activity"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let verbs: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["verb"].as_str().unwrap())
        .collect();
    assert!(
        verbs.contains(&"closed"),
        "activity must contain verb=closed after pair-merge; verbs: {verbs:?}"
    );
}

// ── priority_change records old_value / new_value ─────────────────────────────

#[tokio::test]
async fn priority_change_records_old_new() {
    let h = test_app().await;
    let app = h.router.clone();
    let token = h.admin_token.clone();

    let (s, ev) = post_json(
        app.clone(),
        &token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"Priority log"}}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let task_id = extract_task_id(&ev);

    let (s, _) = post_json(
        app.clone(),
        &token,
        "/v1/commands",
        &json!({"command": {"type": "set_priority", "id": task_id, "priority": "p0"}}).to_string(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, body) = get_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/activity"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let item = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["verb"].as_str() == Some("priority_changed"))
        .expect("must have a priority_changed row");

    assert_eq!(item["field"].as_str().unwrap(), "priority");
    assert!(
        item["old_value"].as_str().is_some(),
        "old_value must be present"
    );
    assert_eq!(item["new_value"].as_str().unwrap(), "p0");
}

// ── add_comment pair-merges into verb = commented ─────────────────────────────

#[tokio::test]
async fn add_comment_merges_into_commented() {
    let h = test_app().await;
    let app = h.router.clone();
    let token = h.admin_token.clone();

    let (s, ev) = post_json(
        app.clone(),
        &token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"Comment activity"}}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let task_id = extract_task_id(&ev);

    // CommentAdded (seq N) + TaskCommented (seq N+1) → pair-merges to verb=commented.
    let (s, _) = post_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/comments"),
        r#"{"body":"looks good to me"}"#,
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);

    let (s, body) = get_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/activity"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let verbs: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["verb"].as_str().unwrap())
        .collect();
    assert!(
        verbs.contains(&"commented"),
        "activity must contain verb=commented; verbs: {verbs:?}"
    );
}

// ── AC-4a: missing task:read capability → 403 ─────────────────────────────────

#[tokio::test]
async fn forbidden_without_task_read_capability() {
    let h = test_app().await;
    let app = h.router.clone();
    let admin = h.admin_token.clone();

    let (s, ev) = post_json(
        app.clone(),
        &admin,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"Auth gate"}}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let task_id = extract_task_id(&ev);

    // Mint a zero-capability token via the admin tokens endpoint.
    let (s, resp) = post_json(
        app.clone(),
        &admin,
        "/v1/tokens",
        r#"{"kind":"svc","agent_id":"00000000-0000-0000-0000-000000000001","capabilities":0,"rate_limit_per_min":60}"#,
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "token mint must succeed");
    let no_cap_token = resp["secret"].as_str().unwrap().to_owned();

    let (s, _) = get_json(
        app.clone(),
        &no_cap_token,
        &format!("/v1/tasks/{task_id}/activity"),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "missing task:read must yield 403");
}

// ── AC-4b: token scoped to wrong project → 403 ────────────────────────────────

#[tokio::test]
async fn forbidden_without_project_in_scope() {
    let h = test_app().await;
    let app = h.router.clone();
    let admin = h.admin_token.clone();

    // Create a project, then a task inside it so project_id is populated.
    let (s, ev) = post_json(
        app.clone(),
        &admin,
        "/v1/commands",
        r#"{"command":{"type":"create_project","title":"Alpha"}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let project_id = extract_project_id(&ev);

    let (s, ev) = post_json(
        app.clone(),
        &admin,
        "/v1/commands",
        &json!({
            "command": {
                "type": "create_task",
                "task": {"title": "Scoped task", "project_id": project_id}
            }
        })
        .to_string(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let task_id = extract_task_id(&ev);

    // Mint a token with TaskRead (bit 1) but scoped to a *different* project.
    let other_proj = "00000000-0000-0000-0000-000000000099";
    let (s, resp) = post_json(
        app.clone(),
        &admin,
        "/v1/tokens",
        &json!({
            "kind": "svc",
            "agent_id": "00000000-0000-0000-0000-000000000002",
            "capabilities": 1,
            "rate_limit_per_min": 60,
            "projects": {"kind": "only", "projects": [other_proj]}
        })
        .to_string(),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::CREATED,
        "scoped token mint must succeed; got {resp}"
    );
    let scoped_token = resp["secret"].as_str().unwrap().to_owned();

    let (s, _) = get_json(
        app.clone(),
        &scoped_token,
        &format!("/v1/tasks/{task_id}/activity"),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "wrong project scope must yield 403"
    );
}

// ── unknown task → 404 ────────────────────────────────────────────────────────

#[tokio::test]
async fn unknown_task_returns_404() {
    let h = test_app().await;
    let fake_id = TaskId::new();
    let (s, _) = get_json(
        h.router.clone(),
        &h.admin_token,
        &format!("/v1/tasks/{fake_id}/activity"),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "unknown task must yield 404");
}

// ── AC-6: cursor pagination ────────────────────────────────────────────────────

#[tokio::test]
async fn pagination_works() {
    let h = test_app().await;
    let app = h.router.clone();
    let token = h.admin_token.clone();

    let (s, ev) = post_json(
        app.clone(),
        &token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"Pagination task"}}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let task_id = extract_task_id(&ev);

    // Append 9 more activity rows by toggling priority.
    for i in 0..9u32 {
        let priority = if i % 2 == 0 { "p0" } else { "p3" };
        let (s, _) = post_json(
            app.clone(),
            &token,
            "/v1/commands",
            &json!({"command": {"type": "set_priority", "id": task_id, "priority": priority}})
                .to_string(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    // Page 1: limit=3.
    let (s, page1) = get_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/activity?limit=3"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let items1 = page1["items"].as_array().unwrap();
    assert_eq!(items1.len(), 3, "page 1 must contain 3 rows");
    assert_eq!(page1["has_more"], true, "has_more must be true on page 1");
    let cursor = page1["next_cursor"]
        .as_u64()
        .expect("next_cursor must be present when has_more=true");

    // Page 2: advance cursor.
    let (s, page2) = get_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/activity?limit=3&cursor={cursor}"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let items2 = page2["items"].as_array().unwrap();
    assert_eq!(items2.len(), 3, "page 2 must contain 3 rows");

    // Verify strict ordering: page 2 seqs must be strictly after page 1.
    let last_seq_p1 = items1.last().unwrap()["seq"].as_i64().unwrap();
    let first_seq_p2 = items2.first().unwrap()["seq"].as_i64().unwrap();
    assert!(
        first_seq_p2 > last_seq_p1,
        "page 2 must begin after page 1 (seq {first_seq_p2} > {last_seq_p1})"
    );
}

// ── AC-7: verb filter ─────────────────────────────────────────────────────────

#[tokio::test]
async fn verb_filter_returns_only_matching() {
    let h = test_app().await;
    let app = h.router.clone();
    let token = h.admin_token.clone();

    let (s, ev) = post_json(
        app.clone(),
        &token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"Filter task"}}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let task_id = extract_task_id(&ev);

    // Add a priority_changed row alongside the created row.
    let (s, _) = post_json(
        app.clone(),
        &token,
        "/v1/commands",
        &json!({"command": {"type": "set_priority", "id": task_id, "priority": "p0"}}).to_string(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // Filter by created → only one row.
    let (s, body) = get_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/activity?verbs=created"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "filter=created must return exactly one row");
    assert_eq!(items[0]["verb"].as_str().unwrap(), "created");

    // Unknown verb → 400.
    let (s, _) = get_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/activity?verbs=no_such_verb"),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "unknown verb must return 400");
}

// ── deleted task still returns audit rows ─────────────────────────────────────

#[tokio::test]
async fn deleted_task_still_returns_activity() {
    let h = test_app().await;
    let app = h.router.clone();
    let token = h.admin_token.clone();

    let (s, ev) = post_json(
        app.clone(),
        &token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"Ephemeral"}}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let task_id = extract_task_id(&ev);

    // Hard-delete the task — removed from tasks projection, audit rows preserved.
    let (s, _) = post_json(
        app.clone(),
        &token,
        "/v1/commands",
        &json!({"command": {"type": "delete_task", "id": task_id}}).to_string(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, body) = get_json(
        app.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/activity"),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "deleted task must still return 200 from activity"
    );

    let items = body["items"].as_array().unwrap();
    assert!(
        !items.is_empty(),
        "audit trail must not be empty after delete"
    );

    let verbs: Vec<&str> = items.iter().map(|i| i["verb"].as_str().unwrap()).collect();
    assert!(
        verbs.contains(&"deleted"),
        "audit trail must include verb=deleted; verbs: {verbs:?}"
    );
}
