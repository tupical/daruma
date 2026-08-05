//! `GET /v1/tasks/{id}/can_start` must consult the same lifecycle gate the
//! `set_status in_progress` transition passes.
//!
//! The core-level coverage (`daruma-core`, `tests/rule_engine.rs`) hands the
//! gate to `can_start` by hand. This file covers the seam that decision is
//! actually made on in production — the route reaching into
//! `handler.lifecycle_gate` — because that is where the defect could quietly
//! come back: pass `None` there and every core test still goes green.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::StatusCode;
use daruma_core::lifecycle_gate::{
    GateCheck, GateDecision, GateOverride, LifecycleGate, TriggerEvent,
};
use daruma_domain::Actor;
use serde_json::json;

mod common;
use common::{json_get, json_post, TestAppBuilder};

/// Blocks `task.before_start`, and reports the rule the way `RuleEngineGate`
/// does — as a structured outcome list.
struct BlockStartGate;

#[async_trait]
impl LifecycleGate for BlockStartGate {
    async fn check(
        &self,
        _actor: &Actor,
        check: &GateCheck,
        _gate_override: &GateOverride,
    ) -> daruma_shared::Result<GateDecision> {
        if check.trigger == TriggerEvent::TaskBeforeStart {
            return Ok(GateDecision::Blocked {
                message: "acceptance criteria required".to_string(),
                details: json!({
                    "outcomes": [{
                        "rule_id": "rule_1",
                        "rule_key": "acceptance-criteria",
                        "decision": "blocked",
                        "message": "acceptance criteria required",
                    }]
                }),
            });
        }
        Ok(GateDecision::Allowed)
    }
}

async fn create_task(app: &axum::Router, token: &str) -> String {
    let (status, ev) = json_post(
        app.clone(),
        token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"Gated"}}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create task: {ev}");
    ev.pointer("/data/events/0/payload/task/id")
        .or_else(|| ev.pointer("/data/0/payload/task/id"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("task id in {ev}"))
        .to_string()
}

#[tokio::test]
async fn can_start_route_reports_the_gate_that_would_block_the_transition() {
    let app = TestAppBuilder::default()
        .lifecycle_gate(Arc::new(BlockStartGate))
        .build()
        .await;
    let token = app.admin_token.clone();
    let task_id = create_task(&app.router, &token).await;

    let (status, readiness) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/can_start"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "can_start: {readiness}");
    assert_eq!(
        readiness["ready"],
        json!(false),
        "the route must consult the gate: {readiness}"
    );
    assert_eq!(readiness["blockers"], json!([]), "no relation blockers here");
    assert_eq!(
        readiness["rule_blockers"][0]["rule_key"],
        json!("acceptance-criteria"),
        "the blocking rule must be named: {readiness}"
    );

    // And the answer is true: the transition really is refused.
    let (status, body) = json_post(
        app.router.clone(),
        &token,
        "/v1/commands",
        &json!({"command": {"type": "set_status", "id": task_id, "status": "in_progress"}})
            .to_string(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "not-ready must mean the transition is blocked: {body}"
    );
}

/// Without a gate the answer is relation-only — the pre-existing behaviour, so
/// a workspace with no rules keeps the response it always had.
#[tokio::test]
async fn can_start_route_without_a_gate_is_ready_and_omits_rule_fields() {
    let app = TestAppBuilder::default().build().await;
    let token = app.admin_token.clone();
    let task_id = create_task(&app.router, &token).await;

    let (status, readiness) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/tasks/{task_id}/can_start"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "can_start: {readiness}");
    assert_eq!(readiness["ready"], json!(true));
    assert_eq!(readiness["reason"], json!("ready"));
    assert!(
        readiness.get("rule_blockers").is_none() && readiness.get("rule_warnings").is_none(),
        "empty rule lists stay off the wire, so old clients see the old shape: {readiness}"
    );
}
