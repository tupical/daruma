//! Drain compensation e2e (docs/LIFECYCLE_RULES_SPEC.md §3, invariant 7).
//!
//! `plan_drain_next` acquires the claim BEFORE the gated `SetStatus`
//! transition. When a lifecycle gate blocks `task.before_start`, the drain
//! must release the claim (compensation in `drain_one_plan`) so the task is
//! not left claimed-but-idle and other agents can still pick it up.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::Value;
use daruma_auth::{Capabilities, Capability, ProjectFilter};
use daruma_core::lifecycle_gate::{
    GateCheck, GateDecision, GateOverride, LifecycleGate, TriggerEvent,
};
use daruma_domain::Actor;
use tower::ServiceExt;

mod common;
use common::{mint_pat, TestAppBuilder};

/// Blocks `task.before_start` while the flag is set; everything else passes.
struct ToggleGate {
    block_before_start: AtomicBool,
}

#[async_trait]
impl LifecycleGate for ToggleGate {
    async fn check(
        &self,
        _actor: &Actor,
        check: &GateCheck,
        _gate_override: &GateOverride,
    ) -> daruma_shared::Result<GateDecision> {
        if check.trigger == TriggerEvent::TaskBeforeStart
            && self.block_before_start.load(Ordering::SeqCst)
        {
            return Ok(GateDecision::Blocked {
                message: "task.before_start requires evidence".to_string(),
                details: serde_json::Value::Null,
            });
        }
        Ok(GateDecision::Allowed)
    }
}

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

#[tokio::test]
async fn blocked_drain_releases_claim_for_other_agents() {
    let gate = Arc::new(ToggleGate {
        block_before_start: AtomicBool::new(true),
    });
    let h = TestAppBuilder::default()
        .lifecycle_gate(gate.clone())
        .build()
        .await;
    let admin = &h.admin_token;

    // Project + active plan with one ready task.
    let (_s, ev) = post_json(
        &h.router,
        admin,
        "/v1/commands",
        r#"{"command":{"type":"create_project","title":"Gated drain"}}"#,
    )
    .await;
    let pid = extract_id(&ev, "project_created", "project");

    let plan_body =
        format!(r#"{{"plan":{{"project_id":"{pid}","title":"Gated","owner":{{"kind":"user"}}}}}}"#);
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

    let (_s, ev) = post_json(
        &h.router,
        admin,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"gated-task"}}}"#,
    )
    .await;
    let task_id = extract_id(&ev, "task_created", "task");

    let (s, r) = post_json(
        &h.router,
        admin,
        &format!("/v1/plans/{plan_id}/tasks"),
        &format!(r#"{{"task_id":"{task_id}"}}"#),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "add_plan_task failed: {r}");
    let (s, r) = post_json(
        &h.router,
        admin,
        &format!("/v1/plans/{plan_id}/status"),
        r#"{"status":"active"}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "activate plan failed: {r}");

    let caps: Capabilities = [Capability::PlanRead, Capability::RunWrite].into();

    // Agent A: gate blocks task.before_start → drain fails with rule_blocked.
    let (tok_a, _agent_a) = mint_pat(&h.auth_store(), caps.clone(), ProjectFilter::All).await;
    let (status_a, body_a) = post_json(
        &h.router,
        &tok_a,
        &format!("/v1/plans/{plan_id}/drain-next"),
        "{}",
    )
    .await;
    assert_eq!(
        status_a,
        StatusCode::CONFLICT,
        "expected rule_blocked conflict, got {status_a}: {body_a}"
    );
    assert!(
        body_a.to_string().contains("rule_blocked"),
        "error body must mention rule_blocked: {body_a}"
    );

    // Gate off → agent B drains the SAME task. This succeeds only if A's
    // claim was released by the compensation path; otherwise the resolver
    // skips the task as claimed-by-another-agent and returns null.
    gate.block_before_start.store(false, Ordering::SeqCst);
    let (tok_b, _agent_b) = mint_pat(&h.auth_store(), caps, ProjectFilter::All).await;
    let (status_b, body_b) = post_json(
        &h.router,
        &tok_b,
        &format!("/v1/plans/{plan_id}/drain-next"),
        "{}",
    )
    .await;
    assert_eq!(
        status_b,
        StatusCode::OK,
        "drain by agent B failed: {body_b}"
    );
    assert_eq!(
        body_b["task_id"].as_str(),
        Some(task_id.as_str()),
        "agent B must receive the previously blocked task: {body_b}"
    );
}
