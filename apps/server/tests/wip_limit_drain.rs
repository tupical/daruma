//! ADR-0015 validation gate 4: a project-scoped `wip_limit` rule on
//! `task.before_start` fires on the drain path too, not only on a direct
//! `SetStatus`. Real `RuleEngineGate` over the server's projections; two
//! agents drain the same plan under a ceiling of one.

use axum::http::StatusCode;
use daruma_auth::{Capabilities, Capability, ProjectFilter};
use serde_json::{json, Value};

mod common;
use common::{json_get, json_post, mint_pat, TestAppBuilder};

fn created_id(ev: &Value, event_type: &str, entity: &str) -> String {
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
async fn project_wip_limit_blocks_the_second_drain_until_a_slot_frees() {
    let h = TestAppBuilder::default().rule_engine_gate().build().await;
    let admin = h.admin_token.clone();
    let post = |uri: String, body: Value| {
        let router = h.router.clone();
        let admin = admin.clone();
        async move { json_post(router, &admin, &uri, &body.to_string()).await }
    };

    let (_s, ev) = post(
        "/v1/commands".into(),
        json!({"command": {"type": "create_project", "title": "WIP pilot"}}),
    )
    .await;
    let pid = created_id(&ev, "project_created", "project");
    post(
        "/v1/plans".into(),
        json!({"plan": {"project_id": pid, "title": "Drain", "owner": {"kind": "user"}}}),
    )
    .await;
    let (_s, plans) = json_get(
        h.router.clone(),
        &admin,
        &format!("/v1/plans?project_id={pid}&status=all"),
    )
    .await;
    let plan_id = plans[0]["id"].as_str().unwrap().to_owned();
    let mut tasks = Vec::new();
    for title in ["first", "second"] {
        let (_s, ev) = post(
            "/v1/commands".into(),
            json!({"command": {"type": "create_task",
                "task": {"title": title, "status": "todo", "project_id": pid}}}),
        )
        .await;
        let task_id = created_id(&ev, "task_created", "task");
        let (s, r) = post(
            format!("/v1/plans/{plan_id}/tasks"),
            json!({"task_id": task_id}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "add plan task: {r}");
        tasks.push(task_id);
    }
    let (s, r) = post(
        format!("/v1/plans/{plan_id}/status"),
        json!({"status": "active"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "activate plan: {r}");
    let (s, r) = post(
        "/v1/rules".into(),
        json!({"rule": {
            "rule_key": "wip-ceiling",
            "title": "One writer per repository",
            "scope": {"kind": "project", "id": pid},
            "trigger": "task.before_start",
            "requirement": {"type": "wip_limit", "max_in_progress": 1},
            "mode": "required",
            "override_allowed": false
        }}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "create wip rule: {r}");

    let caps: Capabilities = [Capability::PlanRead, Capability::RunWrite].into();
    let drain = |token: String| {
        let router = h.router.clone();
        let uri = format!("/v1/plans/{plan_id}/drain-next");
        async move { json_post(router, &token, &uri, "{}").await }
    };

    let (tok_a, _) = mint_pat(&h.auth_store(), caps.clone(), ProjectFilter::All).await;
    let (s, body) = drain(tok_a).await;
    assert_eq!(s, StatusCode::OK, "first drain fits the ceiling: {body}");
    assert_eq!(body["task_id"].as_str(), Some(tasks[0].as_str()), "{body}");

    let (tok_b, _) = mint_pat(&h.auth_store(), caps, ProjectFilter::All).await;
    let (s, body) = drain(tok_b.clone()).await;
    assert_eq!(s, StatusCode::CONFLICT, "second drain is blocked: {body}");
    let text = body.to_string();
    assert!(text.contains("rule_blocked"), "{text}");
    assert!(text.contains("wip_limit"), "{text}");
    assert!(
        text.contains(&tasks[0]),
        "the block names the occupying task: {text}"
    );

    // The occupant finishes → the slot frees and B drains the second task
    // (its claim was compensated on the blocked attempt).
    let (s, r) = post(
        "/v1/commands".into(),
        json!({"command": {"type": "set_status", "id": tasks[0], "status": "done", "force": true}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "finish the occupant: {r}");
    let (s, body) = drain(tok_b).await;
    assert_eq!(s, StatusCode::OK, "a freed slot lets the drain through: {body}");
    assert_eq!(body["task_id"].as_str(), Some(tasks[1].as_str()), "{body}");
}
