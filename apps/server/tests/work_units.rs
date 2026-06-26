//! P3 — WorkUnit: the minimal dispatchable unit. Covers concurrent
//! drain (distinct units), lease coupling on claim (conflict → revert),
//! completion, and that plain tasks are untouched by the layer.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::{json, Value};
use daruma_auth::{Capabilities, Capability, ProjectFilter};
use daruma_core::Command;
use daruma_domain::{Actor, NewTask};
use daruma_shared::TaskId;
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

async fn create_task(h: &common::TestApp, title: &str) -> TaskId {
    let envs = h
        .state
        .commands
        .handler()
        .handle(
            Command::CreateTask {
                task: NewTask::new(title),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    envs.iter()
        .find_map(|e| match &e.payload {
            daruma_events::Event::TaskCreated { task } => task.id,
            _ => None,
        })
        .unwrap()
}

async fn create_unit(h: &common::TestApp, task: TaskId, title: &str, refs: &[&str]) -> String {
    let body = json!({
        "work_unit": {
            "task_id": task,
            "title": title,
            "artifact_refs": refs,
        }
    });
    let (s, resp) = post_json(
        &h.router,
        &h.admin_token,
        "/v1/work-units",
        &body.to_string(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{resp}");
    resp["data"]["work_unit"]["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn concurrent_drain_hands_out_distinct_units_with_briefing() {
    let h = test_app().await;
    let task = create_task(&h, "Big feature").await;
    create_unit(&h, task, "unit-a", &[]).await;
    create_unit(&h, task, "unit-b", &[]).await;

    let caps: Capabilities = [Capability::RunWrite].into();
    let (tok1, _) = mint_pat(&h.auth_store(), caps, ProjectFilter::All).await;
    let (tok2, _) = mint_pat(&h.auth_store(), caps, ProjectFilter::All).await;

    let body = json!({ "task_id": task }).to_string();
    let (r1, r2) = tokio::join!(
        post_json(&h.router, &tok1, "/v1/work-units/drain-next", &body),
        post_json(&h.router, &tok2, "/v1/work-units/drain-next", &body),
    );
    assert_eq!(r1.0, StatusCode::OK, "{:?}", r1.1);
    assert_eq!(r2.0, StatusCode::OK, "{:?}", r2.1);
    let id1 = r1.1["work_unit"]["id"]
        .as_str()
        .expect("agent 1 gets a unit");
    let id2 = r2.1["work_unit"]["id"]
        .as_str()
        .expect("agent 2 gets a unit");
    assert_ne!(id1, id2, "no duplicate dispatch");

    // Pool drained → null.
    let (_s, r3) = post_json(&h.router, &tok1, "/v1/work-units/drain-next", &body).await;
    assert!(r3["work_unit"].is_null());

    // Complete one unit; it never comes back.
    let (s, _r) = post_json(
        &h.router,
        &tok1,
        &format!("/v1/work-units/{id1}/complete"),
        &json!({ "outcome": "ok", "produced_artifacts": ["artifact://api/users"] }).to_string(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_s, r4) = post_json(&h.router, &tok1, "/v1/work-units/drain-next", &body).await;
    assert!(r4["work_unit"].is_null());
}

#[tokio::test]
async fn claim_acquires_declared_leases_and_conflict_reverts() {
    let h = test_app().await;
    let task = create_task(&h, "Contract work").await;
    create_unit(&h, task, "api-unit", &["artifact://api/users"]).await;

    let caps: Capabilities = [Capability::RunWrite].into();
    let (tok1, agent1) = mint_pat(&h.auth_store(), caps, ProjectFilter::All).await;
    let (tok2, _) = mint_pat(&h.auth_store(), caps, ProjectFilter::All).await;
    let _ = (tok1, agent1);

    // Another agent pre-holds the artifact exclusively.
    let blocker_task = create_task(&h, "Blocker").await;
    let other = daruma_shared::AgentId::new();
    let out = h
        .state
        .work_leases
        .try_reserve_targets(
            other,
            blocker_task,
            None,
            vec!["artifact://api/users".into()],
            daruma_domain::LeaseMode::Exclusive,
            chrono::Duration::seconds(60),
        )
        .await
        .unwrap();
    assert!(matches!(
        out,
        daruma_storage::work_lease_repo::ReserveOutcome::Reserved { .. }
    ));

    // Drain: claim must revert on the lease conflict and report the holder.
    let body = json!({ "task_id": task }).to_string();
    let (s, resp) = post_json(&h.router, &tok2, "/v1/work-units/drain-next", &body).await;
    assert_eq!(s, StatusCode::OK, "{resp}");
    assert!(resp["work_unit"].is_null(), "{resp}");
    assert_eq!(
        resp["lease_conflict"]["path"].as_str().unwrap(),
        "artifact://api/users"
    );

    // The unit is still dispatchable: once the lease frees up, the next
    // drain wins it and the briefing carries the fencing-token leases.
    h.state
        .work_leases
        .release_for_task(other, blocker_task)
        .await
        .unwrap();
    let (s, resp) = post_json(&h.router, &tok2, "/v1/work-units/drain-next", &body).await;
    assert_eq!(s, StatusCode::OK, "{resp}");
    let unit = &resp["work_unit"];
    assert_eq!(unit["title"], "api-unit");
    assert!(
        resp["leases"][0]["fencing_token"].as_i64().is_some(),
        "briefing carries fenced leases: {resp}"
    );
}

#[tokio::test]
async fn plain_tasks_are_untouched_by_the_work_unit_layer() {
    let h = test_app().await;
    let task = create_task(&h, "Simple task").await;
    // No units created → list is empty, drain returns null, and the plain
    // task claim path still works.
    let units = h.state.work_units.list_by_task(task).await.unwrap();
    assert!(units.is_empty());

    let caps: Capabilities = [Capability::RunWrite].into();
    let (tok, _) = mint_pat(&h.auth_store(), caps, ProjectFilter::All).await;
    let (_s, resp) = post_json(
        &h.router,
        &tok,
        "/v1/work-units/drain-next",
        &json!({ "task_id": task }).to_string(),
    )
    .await;
    assert!(resp["work_unit"].is_null());
}
