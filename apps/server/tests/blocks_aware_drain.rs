//! P2 (WorkUnit + Artifact Ownership): the claim-aware resolver must honor
//! cross-task `Blocks` relations, not just `plan_tasks.depends_on` — without
//! this, concurrent agents can each grab one side of a mutually-blocking
//! pair and deadlock against each other.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::Value;
use daruma_auth::{Capabilities, Capability, ProjectFilter};
use tower::ServiceExt;

mod common;
use common::{mint_pat, test_app};

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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn extract_id(ev: &Value, event_type: &str, entity: &str) -> String {
    ev["data"]
        .as_array()
        .expect("data array")
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == event_type {
                p.get(entity)?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("{event_type} event with {entity}.id in {ev}"))
}

async fn setup_plan_with_blocks() -> (common::TestApp, String, String, String) {
    let h = test_app().await;
    let admin = h.admin_token.clone();

    let (_s, ev) = post_json(
        &h.router,
        &admin,
        "/v1/commands",
        r#"{"command":{"type":"create_project","title":"Blocks Drain"}}"#,
    )
    .await;
    let pid = extract_id(&ev, "project_created", "project");

    let mk = |title: &str| {
        format!(
            r#"{{"command":{{"type":"create_task","task":{{"title":"{title}","project_id":"{pid}"}}}}}}"#
        )
    };
    let (_s, ev) = post_json(&h.router, &admin, "/v1/commands", &mk("blocked-by-a")).await;
    let task_b = extract_id(&ev, "task_created", "task");
    let (_s, ev) = post_json(&h.router, &admin, "/v1/commands", &mk("blocker-a")).await;
    let task_a = extract_id(&ev, "task_created", "task");

    // A blocks B — a cross-task relation, NOT a plan-level depends_on.
    let link = format!(r#"{{"from":"{task_a}","to":"{task_b}","kind":"blocks"}}"#);
    let (s, r) = post_json(&h.router, &admin, "/v1/relations", &link).await;
    assert!(s.is_success(), "link failed: {r}");

    // Plan lists B FIRST so position order alone would hand B out.
    let plan_body =
        format!(r#"{{"plan":{{"project_id":"{pid}","title":"P","owner":{{"kind":"user"}}}}}}"#);
    post_json(&h.router, &admin, "/v1/plans", &plan_body).await;
    let (_s, list) = get_json(
        &h.router,
        &admin,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let plan_id = list.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for tid in [&task_b, &task_a] {
        let (s, r) = post_json(
            &h.router,
            &admin,
            &format!("/v1/plans/{plan_id}/tasks"),
            &format!(r#"{{"task_id":"{tid}"}}"#),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "add task failed: {r}");
    }
    let (s, r) = post_json(
        &h.router,
        &admin,
        &format!("/v1/plans/{plan_id}/status"),
        r#"{"status":"active"}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "activate failed: {r}");

    (h, plan_id, task_a, task_b)
}

#[tokio::test]
async fn drain_skips_tasks_with_active_blocks_relations() {
    let (h, plan_id, task_a, task_b) = setup_plan_with_blocks().await;

    let caps: Capabilities = [Capability::PlanRead, Capability::RunWrite].into();
    let (tok1, _) = mint_pat(&h.auth_store(), caps, ProjectFilter::All).await;
    let (tok2, _) = mint_pat(&h.auth_store(), caps, ProjectFilter::All).await;

    // Two agents drain concurrently. B is positioned first but blocked by A:
    // exactly one agent must get A and the other must get null — nobody may
    // hold B while A is open.
    let drain_uri = format!("/v1/plans/{plan_id}/drain-next");
    let (r1, r2) = tokio::join!(
        post_json(&h.router, &tok1, &drain_uri, "{}"),
        post_json(&h.router, &tok2, &drain_uri, "{}"),
    );
    let granted: Vec<&Value> = [&r1.1, &r2.1]
        .into_iter()
        .filter(|v| !v.is_null())
        .collect();
    assert_eq!(
        granted.len(),
        1,
        "exactly one agent gets work: {r1:?} {r2:?}"
    );
    assert_eq!(
        granted[0]["task_id"].as_str().unwrap(),
        task_a,
        "the blocker A must be handed out, never the blocked B"
    );

    // Complete A → B becomes eligible on the next drain.
    let admin = &h.admin_token;
    let (s, r) = post_json(
        &h.router,
        admin,
        "/v1/commands",
        &format!(r#"{{"command":{{"type":"complete_task","id":"{task_a}"}}}}"#),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "complete A failed: {r}");

    let (s, resp) = post_json(
        &h.router,
        &tok2,
        &format!("/v1/plans/{plan_id}/drain-next"),
        "{}",
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        resp["task_id"].as_str().unwrap(),
        task_b,
        "after the blocker closes, B is dispatched: {resp}"
    );
}

#[tokio::test]
async fn plan_next_task_respects_blocks_too() {
    let (h, plan_id, task_a, _task_b) = setup_plan_with_blocks().await;
    let admin = &h.admin_token;

    let run_id = uuid::Uuid::now_v7();
    let (s, resp) = get_json(
        &h.router,
        admin,
        &format!("/v1/plans/{plan_id}/next-task?run_id={run_id}"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        resp["task_id"].as_str().unwrap(),
        task_a,
        "peek path must skip the blocked candidate as well: {resp}"
    );
}
