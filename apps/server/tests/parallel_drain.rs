//! End-to-end test: concurrent `drain_next` never hands the same task to two
//! agents. Verifies the claim-aware resolver + atomic compare-and-set so a fleet
//! of agents can "close all tasks for a project" in parallel without collisions.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::Value;
use std::collections::HashSet;
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

async fn create_project(app: &axum::Router, token: &str) -> String {
    let (_s, ev) = post_json(
        app,
        token,
        "/v1/commands",
        r#"{"command":{"type":"create_project","title":"Parallel Project"}}"#,
    )
    .await;
    extract_id(&ev, "project_created", "project")
}

async fn create_task(app: &axum::Router, token: &str, title: &str) -> String {
    let body = format!(r#"{{"command":{{"type":"create_task","task":{{"title":"{title}"}}}}}}"#);
    let (_s, ev) = post_json(app, token, "/v1/commands", &body).await;
    extract_id(&ev, "task_created", "task")
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

#[tokio::test]
async fn concurrent_drain_assigns_distinct_tasks() {
    let h = test_app().await;
    let admin = &h.admin_token;
    let pid = create_project(&h.router, admin).await;

    // Plan with M tasks, all ready (no deps), plan Active.
    let plan_body =
        format!(r#"{{"plan":{{"project_id":"{pid}","title":"Drain","owner":{{"kind":"user"}}}}}}"#);
    post_json(&h.router, admin, "/v1/plans", &plan_body).await;
    let (_s, list) = get_json(
        &h.router,
        admin,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let plan_id = list.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    const M: usize = 6;
    let mut created = HashSet::new();
    for i in 0..M {
        let task_id = create_task(&h.router, admin, &format!("task-{i}")).await;
        let add = format!(r#"{{"task_id":"{task_id}"}}"#);
        let (s, r) = post_json(
            &h.router,
            admin,
            &format!("/v1/plans/{plan_id}/tasks"),
            &add,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "add_plan_task failed: {r}");
        created.insert(task_id);
    }
    let (s, r) = post_json(
        &h.router,
        admin,
        &format!("/v1/plans/{plan_id}/status"),
        r#"{"status":"active"}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "activate plan failed: {r}");

    // M+1 agents (distinct agent_ids via distinct tokens) drain concurrently.
    let caps: Capabilities = [Capability::PlanRead, Capability::RunWrite].into();
    let mut tokens = Vec::new();
    for _ in 0..M + 1 {
        let (tok, _agent) = mint_pat(&h.auth_store(), caps.clone(), ProjectFilter::All).await;
        tokens.push(tok);
    }

    let futures = tokens.iter().map(|tok| {
        let router = h.router.clone();
        let tok = tok.clone();
        let plan_id = plan_id.clone();
        async move {
            let (s, resp) = post_json(
                &router,
                &tok,
                &format!("/v1/plans/{plan_id}/drain-next"),
                "{}",
            )
            .await;
            assert_eq!(s, StatusCode::OK, "drain-next failed: {resp}");
            resp
        }
    });
    let results = futures::future::join_all(futures).await;

    // Exactly M agents get a task, each distinct; one agent gets null.
    let mut claimed = HashSet::new();
    let mut nulls = 0;
    for resp in &results {
        if resp.is_null() {
            nulls += 1;
        } else {
            let tid = resp["task_id"].as_str().expect("task_id").to_owned();
            assert!(claimed.insert(tid), "task handed to two agents: {resp}");
        }
    }
    assert_eq!(claimed.len(), M, "every task must be claimed exactly once");
    assert_eq!(nulls, 1, "the surplus agent must get null");
    assert_eq!(claimed, created, "claimed set must equal created tasks");
}

/// Project-wide ready drain pulls tasks across *multiple* active plans and never
/// hands the same task to two agents.
#[tokio::test]
async fn project_ready_drain_spans_plans_without_collision() {
    let h = test_app().await;
    let admin = &h.admin_token;
    let pid = create_project(&h.router, admin).await;

    // Two active plans, each with 2 ready tasks → pool of 4.
    let mut created = HashSet::new();
    for p in 0..2 {
        let plan_body = format!(
            r#"{{"plan":{{"project_id":"{pid}","title":"Plan {p}","owner":{{"kind":"user"}}}}}}"#
        );
        post_json(&h.router, admin, "/v1/plans", &plan_body).await;
    }
    let (_s, list) = get_json(
        &h.router,
        admin,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let plan_ids: Vec<String> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(plan_ids.len(), 2);

    for (i, plan_id) in plan_ids.iter().enumerate() {
        for j in 0..2 {
            let task_id = create_task(&h.router, admin, &format!("p{i}-t{j}")).await;
            let add = format!(r#"{{"task_id":"{task_id}"}}"#);
            post_json(
                &h.router,
                admin,
                &format!("/v1/plans/{plan_id}/tasks"),
                &add,
            )
            .await;
            created.insert(task_id);
        }
        post_json(
            &h.router,
            admin,
            &format!("/v1/plans/{plan_id}/status"),
            r#"{"status":"active"}"#,
        )
        .await;
    }

    // 5 agents drain the project pool concurrently → 4 distinct tasks + 1 null.
    let caps: Capabilities = [Capability::PlanRead, Capability::RunWrite].into();
    let mut tokens = Vec::new();
    for _ in 0..5 {
        let (tok, _a) = mint_pat(&h.auth_store(), caps.clone(), ProjectFilter::All).await;
        tokens.push(tok);
    }
    let futures = tokens.iter().map(|tok| {
        let router = h.router.clone();
        let tok = tok.clone();
        let pid = pid.clone();
        async move {
            let (s, resp) = post_json(
                &router,
                &tok,
                &format!("/v1/ready/drain?project_id={pid}"),
                "{}",
            )
            .await;
            assert_eq!(s, StatusCode::OK, "ready/drain failed: {resp}");
            resp
        }
    });
    let results = futures::future::join_all(futures).await;

    let mut claimed = HashSet::new();
    let mut nulls = 0;
    for resp in &results {
        if resp.is_null() {
            nulls += 1;
        } else {
            let tid = resp["task_id"].as_str().expect("task_id").to_owned();
            assert!(claimed.insert(tid), "task handed to two agents: {resp}");
        }
    }
    assert_eq!(
        claimed.len(),
        4,
        "all 4 pool tasks claimed once: {claimed:?}"
    );
    assert_eq!(nulls, 1, "surplus agent gets null");
    assert_eq!(claimed, created);
}
