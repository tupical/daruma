//! End-to-end integration tests for Plan HTTP endpoints (W3.1).
//!
//! Covers:
//!   POST   /v1/plans               → 201 + MutationResponse (success, event_id, event_seq)
//!   GET    /v1/plans/{id}          → 200 + {plan, progress}
//!   GET    /v1/plans?project_id=&status= → 200 array; 400 when project_id or status absent
//!   PATCH  /v1/plans/{id}          → 200 + MutationResponse
//!   Capability gating: PlanWrite / PlanRead enforced

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use daruma_auth::{Capability, ProjectFilter};
use serde_json::Value;
use tower::ServiceExt;

mod common;
use common::{mint_pat, test_app};

// ── HTTP helpers ──────────────────────────────────────────────────────────────

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
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn get_json(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn patch_json(app: &axum::Router, token: &str, uri: &str, body: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::PATCH)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn delete_json(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::DELETE)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Create a project via /v1/commands and return its UUID string.
async fn create_project(app: &axum::Router, token: &str) -> String {
    let (s, ev) = post_json(
        app,
        token,
        "/v1/commands",
        r#"{"command":{"type":"create_project","title":"Test Project"}}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "create_project failed: {ev}");
    ev["data"]
        .as_array()
        .expect("data must be array")
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == "project_created" {
                p.get("project")?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("project_created event with project.id")
}

async fn create_task(app: &axum::Router, token: &str, title: &str) -> String {
    let (status, created) = post_json(
        app,
        token,
        "/v1/commands",
        &serde_json::json!({
            "command": {"type": "create_task", "task": {"title": title}}
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create_task failed: {created}");
    created["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|event| {
            (event["payload"]["type"] == "task_created")
                .then(|| event["payload"]["task"]["id"].as_str().map(str::to_owned))
                .flatten()
        })
        .expect("task_created event with task.id")
}

// ── AC: Plan CRUD ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn plans_create_returns_201_mutation_response() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;

    let body = format!(
        r#"{{"plan":{{"project_id":"{pid}","title":"Sprint 1","owner":{{"kind":"user"}}}}}}"#
    );
    let (s, resp) = post_json(&h.router, &h.admin_token, "/v1/plans", &body).await;

    assert_eq!(s, StatusCode::CREATED, "expected 201: {resp}");
    assert_eq!(resp["success"], true);
    assert!(
        resp["event_id"].is_string(),
        "event_id must be present: {resp}"
    );
    assert!(
        resp["event_seq"].is_number(),
        "event_seq must be present: {resp}"
    );
}

#[tokio::test]
async fn plans_get_returns_plan_and_progress() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;

    let body = format!(
        r#"{{"plan":{{"project_id":"{pid}","title":"Plan A","owner":{{"kind":"user"}}}}}}"#
    );
    post_json(&h.router, &h.admin_token, "/v1/plans", &body).await;

    // List to resolve the plan_id (create response only carries event_id).
    let (ls, list) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    assert_eq!(ls, StatusCode::OK, "list plans failed: {list}");
    let plans = list.as_array().expect("list must be array");
    assert!(!plans.is_empty(), "at least one plan expected after create");
    let plan_id = plans[0]["id"].as_str().expect("plan.id string").to_owned();

    let (gs, plan_resp) =
        get_json(&h.router, &h.admin_token, &format!("/v1/plans/{plan_id}")).await;
    assert_eq!(gs, StatusCode::OK, "get plan failed: {plan_resp}");
    assert_eq!(plan_resp["plan"]["id"], plan_id, "plan id mismatch");
    assert!(
        plan_resp["progress"].is_object(),
        "progress must be an object: {plan_resp}"
    );
}

#[tokio::test]
async fn plans_get_latest_activity_follows_attach_without_task_update() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;

    let body = format!(
        r#"{{"plan":{{"project_id":"{pid}","title":"Activity plan","owner":{{"kind":"user"}}}}}}"#
    );
    post_json(&h.router, &h.admin_token, "/v1/plans", &body).await;
    let (_, list) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let plan_id = list.as_array().unwrap()[0]["id"].as_str().unwrap();

    let (_, created) = post_json(
        &h.router,
        &h.admin_token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"activity task"}}}"#,
    )
    .await;
    let task_id = created["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|event| {
            (event["payload"]["type"] == "task_created")
                .then(|| event["payload"]["task"]["id"].as_str())
                .flatten()
        })
        .unwrap();
    let (status, attached) = post_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{plan_id}/tasks"),
        &format!(r#"{{"task_id":"{task_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "attach failed: {attached}");

    let (_, task) = get_json(&h.router, &h.admin_token, &format!("/v1/tasks/{task_id}")).await;
    let (_, plan) = get_json(&h.router, &h.admin_token, &format!("/v1/plans/{plan_id}")).await;

    assert!(
        plan["latest_activity_at"].as_str().unwrap() > task["updated_at"].as_str().unwrap(),
        "attach-only event must advance plan activity beyond the unchanged task: {plan} task={task}"
    );
}

#[tokio::test]
async fn plans_get_latest_activity_does_not_move_backward_after_removal() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;

    let body = format!(
        r#"{{"plan":{{"project_id":"{pid}","title":"Removal activity plan","owner":{{"kind":"user"}}}}}}"#
    );
    post_json(&h.router, &h.admin_token, "/v1/plans", &body).await;
    let (_, list) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let plan_id = list.as_array().unwrap()[0]["id"].as_str().unwrap();

    let (_, created) = post_json(
        &h.router,
        &h.admin_token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"removal activity task"}}}"#,
    )
    .await;
    let task_id = created["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|event| {
            (event["payload"]["type"] == "task_created")
                .then(|| event["payload"]["task"]["id"].as_str())
                .flatten()
        })
        .unwrap();
    let (status, attached) = post_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{plan_id}/tasks"),
        &format!(r#"{{"task_id":"{task_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "attach failed: {attached}");
    let (_, before) = get_json(&h.router, &h.admin_token, &format!("/v1/plans/{plan_id}")).await;

    let (status, removed) = delete_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{plan_id}/tasks/{task_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "remove failed: {removed}");
    let (_, after) = get_json(&h.router, &h.admin_token, &format!("/v1/plans/{plan_id}")).await;

    assert!(
        after["latest_activity_at"].as_str().unwrap()
            >= before["latest_activity_at"].as_str().unwrap(),
        "removing the latest task must not move plan activity backward: before={before} after={after}"
    );
}

#[tokio::test]
async fn plans_get_latest_activity_follows_reorder() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;

    let body = format!(
        r#"{{"plan":{{"project_id":"{pid}","title":"Reorder activity plan","owner":{{"kind":"user"}}}}}}"#
    );
    post_json(&h.router, &h.admin_token, "/v1/plans", &body).await;
    let (_, list) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let plan_id = list.as_array().unwrap()[0]["id"].as_str().unwrap();
    let first_task = create_task(&h.router, &h.admin_token, "first reorder task").await;
    let second_task = create_task(&h.router, &h.admin_token, "second reorder task").await;
    for task_id in [&first_task, &second_task] {
        let (status, attached) = post_json(
            &h.router,
            &h.admin_token,
            &format!("/v1/plans/{plan_id}/tasks"),
            &format!(r#"{{"task_id":"{task_id}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "attach failed: {attached}");
    }
    let (_, before) = get_json(&h.router, &h.admin_token, &format!("/v1/plans/{plan_id}")).await;

    let (status, reordered) = post_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{plan_id}/reorder"),
        &serde_json::json!({"order": [second_task, first_task]}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "reorder failed: {reordered}");
    let (_, after) = get_json(&h.router, &h.admin_token, &format!("/v1/plans/{plan_id}")).await;

    assert!(
        after["latest_activity_at"].as_str().unwrap()
            > before["latest_activity_at"].as_str().unwrap(),
        "reordering tasks must advance plan activity: before={before} after={after}"
    );
}

#[tokio::test]
async fn plans_get_latest_activity_follows_direct_child_plan_activity() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;

    let body = format!(
        r#"{{"plan":{{"project_id":"{pid}","title":"Parent activity plan","owner":{{"kind":"user"}}}}}}"#
    );
    post_json(&h.router, &h.admin_token, "/v1/plans", &body).await;
    let (_, list) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let parent_id = list.as_array().unwrap()[0]["id"].as_str().unwrap();
    let (_, before) = get_json(&h.router, &h.admin_token, &format!("/v1/plans/{parent_id}")).await;

    let child = format!(
        r#"{{"plan":{{"project_id":"{pid}","parent_plan_id":"{parent_id}","title":"Direct child activity plan","owner":{{"kind":"user"}}}}}}"#
    );
    let (status, created) = post_json(&h.router, &h.admin_token, "/v1/plans", &child).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create child failed: {created}"
    );
    let (_, after) = get_json(&h.router, &h.admin_token, &format!("/v1/plans/{parent_id}")).await;

    assert!(
        after["latest_activity_at"].as_str().unwrap()
            > before["latest_activity_at"].as_str().unwrap(),
        "direct child plan creation must advance parent activity: before={before} after={after}"
    );
}

#[tokio::test]
async fn plans_get_latest_activity_advances_when_direct_child_is_deleted() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;
    let parent_body = format!(
        r#"{{"plan":{{"project_id":"{pid}","title":"Delete child parent","owner":{{"kind":"user"}}}}}}"#
    );
    post_json(&h.router, &h.admin_token, "/v1/plans", &parent_body).await;
    let (_, plans) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let parent_id = plans.as_array().unwrap()[0]["id"].as_str().unwrap();
    let child_body = format!(
        r#"{{"plan":{{"project_id":"{pid}","parent_plan_id":"{parent_id}","title":"Child to delete","owner":{{"kind":"user"}}}}}}"#
    );
    let (status, created) = post_json(&h.router, &h.admin_token, "/v1/plans", &child_body).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create child failed: {created}"
    );
    let (_, plans) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let child_id = plans
        .as_array()
        .unwrap()
        .iter()
        .find(|plan| plan["title"] == "Child to delete")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let (_, before) = get_json(&h.router, &h.admin_token, &format!("/v1/plans/{parent_id}")).await;

    let (status, deleted) =
        delete_json(&h.router, &h.admin_token, &format!("/v1/plans/{child_id}")).await;
    assert_eq!(status, StatusCode::OK, "delete child failed: {deleted}");
    let (_, after) = get_json(&h.router, &h.admin_token, &format!("/v1/plans/{parent_id}")).await;

    assert!(
        after["latest_activity_at"].as_str().unwrap()
            > before["latest_activity_at"].as_str().unwrap(),
        "deleting a direct child must advance former parent activity: before={before} after={after}"
    );
}

#[tokio::test]
async fn plans_get_latest_activity_advances_both_parents_when_child_is_reparented() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;
    for title in ["Former reparent parent", "New reparent parent"] {
        let body = serde_json::json!({
            "plan": {
                "project_id": pid,
                "title": title,
                "owner": {"kind": "user"}
            }
        })
        .to_string();
        let (status, created) = post_json(&h.router, &h.admin_token, "/v1/plans", &body).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "create parent failed: {created}"
        );
    }
    let (_, plans) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let id_for_title = |title: &str| {
        plans
            .as_array()
            .unwrap()
            .iter()
            .find(|plan| plan["title"] == title)
            .unwrap()["id"]
            .as_str()
            .unwrap()
    };
    let former_parent_id = id_for_title("Former reparent parent");
    let new_parent_id = id_for_title("New reparent parent");
    let child_body = serde_json::json!({
        "plan": {
            "project_id": pid,
            "parent_plan_id": former_parent_id,
            "title": "Child to reparent",
            "owner": {"kind": "user"}
        }
    })
    .to_string();
    let (status, created) = post_json(&h.router, &h.admin_token, "/v1/plans", &child_body).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create child failed: {created}"
    );
    let (_, plans) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let child_id = plans
        .as_array()
        .unwrap()
        .iter()
        .find(|plan| plan["title"] == "Child to reparent")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let (_, former_before) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{former_parent_id}"),
    )
    .await;
    let (_, new_before) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{new_parent_id}"),
    )
    .await;

    let (status, updated) = patch_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{child_id}"),
        &serde_json::json!({"patch": {"parent_plan_id": new_parent_id}}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "reparent child failed: {updated}");
    let (_, former_after) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{former_parent_id}"),
    )
    .await;
    let (_, new_after) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{new_parent_id}"),
    )
    .await;

    assert!(
        former_after["latest_activity_at"].as_str().unwrap()
            > former_before["latest_activity_at"].as_str().unwrap(),
        "reparenting away must advance former parent activity: before={former_before} after={former_after}"
    );
    assert!(
        new_after["latest_activity_at"].as_str().unwrap()
            > new_before["latest_activity_at"].as_str().unwrap(),
        "reparenting must advance new parent activity: before={new_before} after={new_after}"
    );
}

#[tokio::test]
async fn plans_get_latest_activity_follows_attached_task_update() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;

    let body = format!(
        r#"{{"plan":{{"project_id":"{pid}","title":"Task update activity plan","owner":{{"kind":"user"}}}}}}"#
    );
    post_json(&h.router, &h.admin_token, "/v1/plans", &body).await;
    let (_, list) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let plan_id = list.as_array().unwrap()[0]["id"].as_str().unwrap();
    let task_id = create_task(&h.router, &h.admin_token, "task before ordinary update").await;
    let (status, attached) = post_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{plan_id}/tasks"),
        &format!(r#"{{"task_id":"{task_id}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "attach failed: {attached}");
    let (_, before) = get_json(&h.router, &h.admin_token, &format!("/v1/plans/{plan_id}")).await;

    let (status, updated) = post_json(
        &h.router,
        &h.admin_token,
        "/v1/commands",
        &serde_json::json!({
            "command": {
                "type": "update_task",
                "id": task_id,
                "patch": {"title": "task after ordinary update"}
            }
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update task failed: {updated}");
    let (_, after) = get_json(&h.router, &h.admin_token, &format!("/v1/plans/{plan_id}")).await;

    assert!(
        after["latest_activity_at"].as_str().unwrap()
            > before["latest_activity_at"].as_str().unwrap(),
        "ordinary attached task update must advance plan activity: before={before} after={after}"
    );
}

#[tokio::test]
async fn plans_get_accepts_stable_slug_url_after_rename() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;

    let body = format!(
        r#"{{"plan":{{"project_id":"{pid}","title":"Original Roadmap","owner":{{"kind":"user"}}}}}}"#
    );
    post_json(&h.router, &h.admin_token, "/v1/plans", &body).await;

    let (_, list) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let plan_id = list.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let old_slug = format!("original-roadmap-{plan_id}");

    let (s, plan_resp) =
        get_json(&h.router, &h.admin_token, &format!("/v1/plans/{old_slug}")).await;
    assert_eq!(s, StatusCode::OK, "slug URL must resolve: {plan_resp}");
    assert_eq!(plan_resp["plan"]["id"], plan_id);
    assert_eq!(plan_resp["slug"], old_slug);

    let (ps, patch_resp) = patch_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{plan_id}"),
        r#"{"patch":{"title":"Renamed Roadmap"}}"#,
    )
    .await;
    assert_eq!(ps, StatusCode::OK, "patch must return 200: {patch_resp}");

    let (s, renamed) = get_json(&h.router, &h.admin_token, &format!("/v1/plans/{old_slug}")).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "old slug URL must keep resolving: {renamed}"
    );
    assert_eq!(renamed["plan"]["title"], "Renamed Roadmap");
    assert_eq!(renamed["slug"], format!("renamed-roadmap-{plan_id}"));
}

#[tokio::test]
async fn plans_list_without_project_id_returns_400() {
    let h = test_app().await;

    let (s, _resp) = get_json(&h.router, &h.admin_token, "/v1/plans").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "project_id is required");
}

#[tokio::test]
async fn plans_list_without_status_returns_400() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;

    let (s, _resp) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans?project_id={pid}"),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "status is required");
}

#[tokio::test]
async fn plans_list_by_project_returns_created_plan() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;

    let body = format!(
        r#"{{"plan":{{"project_id":"{pid}","title":"Plan B","owner":{{"kind":"user"}}}}}}"#
    );
    post_json(&h.router, &h.admin_token, "/v1/plans", &body).await;

    let (ls, list) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    assert_eq!(ls, StatusCode::OK);
    let plans = list.as_array().expect("list must be array");
    assert!(
        plans.iter().any(|p| p["title"] == "Plan B"),
        "Plan B must appear in list: {plans:?}"
    );
}

#[tokio::test]
async fn plans_update_title() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;

    let body = format!(
        r#"{{"plan":{{"project_id":"{pid}","title":"Old Title","owner":{{"kind":"user"}}}}}}"#
    );
    post_json(&h.router, &h.admin_token, "/v1/plans", &body).await;

    let (_, list) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let plan_id = list.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let (ps, patch_resp) = patch_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans/{plan_id}"),
        r#"{"patch":{"title":"New Title"}}"#,
    )
    .await;
    assert_eq!(ps, StatusCode::OK, "patch must return 200: {patch_resp}");
    assert_eq!(patch_resp["success"], true);
}

// ── AC: Capability gating ─────────────────────────────────────────────────────

#[tokio::test]
async fn plans_create_requires_plan_write_capability() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;

    let (read_only, _) = mint_pat(
        &h.auth_store(),
        [Capability::PlanRead].into(),
        ProjectFilter::All,
    )
    .await;
    let body =
        format!(r#"{{"plan":{{"project_id":"{pid}","title":"Gated","owner":{{"kind":"user"}}}}}}"#);
    let (s, resp) = post_json(&h.router, &read_only, "/v1/plans", &body).await;

    assert_eq!(s, StatusCode::FORBIDDEN, "plan:read cannot create: {resp}");
    assert_eq!(
        resp["error"]["code"], "forbidden",
        "error code must be forbidden: {resp}"
    );
}

#[tokio::test]
async fn plans_get_requires_plan_read_capability() {
    let h = test_app().await;
    let pid = create_project(&h.router, &h.admin_token).await;

    let body = format!(
        r#"{{"plan":{{"project_id":"{pid}","title":"ReadGated","owner":{{"kind":"user"}}}}}}"#
    );
    post_json(&h.router, &h.admin_token, "/v1/plans", &body).await;

    // Get the plan_id using the admin token.
    let (_, list) = get_json(
        &h.router,
        &h.admin_token,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let plan_id = list.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // A token that has PlanWrite but NOT PlanRead must get 403 on GET.
    let (write_only, _) = mint_pat(
        &h.auth_store(),
        [Capability::PlanWrite].into(),
        ProjectFilter::All,
    )
    .await;
    let (s, resp) = get_json(&h.router, &write_only, &format!("/v1/plans/{plan_id}")).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "plan:write cannot read: {resp}");
}
