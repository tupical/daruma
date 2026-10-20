//! MCP tool integration tests for plan tools.
//!
//! Regression cover for the `daruma_plan_create`/`daruma_plan_update`
//! body-wrapping bug (server expects `{plan: NewPlan, external_ref?}` and
//! `{patch: PlanPatch}` respectively; the shim previously sent flat bodies
//! and got HTTP 422 "missing field `plan`/`patch`").

use std::sync::Arc;

use async_trait::async_trait;
use daruma_api_dto::MutationWarning;
use daruma_core::lifecycle_gate::{
    GateCheck, GateDecision, GateOverride, LifecycleGate, TriggerEvent,
};
use daruma_domain::Actor;
use daruma_mcp::{dispatch_request_with_profile, ApiClient, JsonRpcRequest, ToolProfile};
use serde_json::json;

mod common;
use common::{spawn_server, test_app, TestAppBuilder};

async fn spawn_daruma_inline() -> (std::net::SocketAddr, String) {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    (addr, app.admin_token)
}

struct PlanOverrideGate;

#[async_trait]
impl LifecycleGate for PlanOverrideGate {
    async fn check(
        &self,
        _actor: &Actor,
        check: &GateCheck,
        gate_override: &GateOverride,
    ) -> daruma_shared::Result<GateDecision> {
        if check.trigger != TriggerEvent::PlanBeforeApprove {
            return Ok(GateDecision::Allowed);
        }
        if gate_override.force
            && gate_override.override_reason.as_deref() == Some("urgent production repair")
        {
            return Ok(GateDecision::Warning(vec![MutationWarning {
                code: "rule_warning:plan-approval-proof".into(),
                message: "Read architecture.md before approval.".into(),
                details: json!({"rule_key": "plan-approval-proof"}),
            }]));
        }
        Ok(GateDecision::Blocked {
            message: "Read architecture.md before approval.".into(),
            details: json!({"rule_key": "plan-approval-proof"}),
        })
    }
}

fn req(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: method.into(),
        params: Some(params),
    }
}

async fn call_tool(
    client: &ApiClient,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let resp = dispatch_request(
        client,
        req(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        ),
    )
    .await
    .unwrap();
    assert!(resp.error.is_none(), "tool {name} failed: {:?}", resp.error);
    let text = resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    serde_json::from_str(&text).unwrap()
}

async fn create_project_via_mcp(client: &ApiClient, title: &str) -> String {
    let resp = call_tool(client, "daruma_project_create", json!({ "title": title })).await;
    resp["project_id"]
        .as_str()
        .expect("project_id must be a string in response")
        .to_owned()
}

async fn create_plan_via_mcp(
    client: &ApiClient,
    project_id: &str,
    title: &str,
) -> serde_json::Value {
    call_tool(
        client,
        "daruma_plan_create",
        json!({ "title": title, "project_id": project_id }),
    )
    .await
}

async fn first_plan_id(client: &ApiClient, project_id: &str) -> String {
    let list = call_tool(
        client,
        "daruma_plan_list",
        json!({ "project_id": project_id, "status": "all" }),
    )
    .await;
    list["items"]
        .as_array()
        .expect("plan list must be array")
        .first()
        .and_then(|p| p["id"].as_str())
        .expect("at least one plan with id")
        .to_owned()
}

// ── Regression: plan_create body wrapper bug ──────────────────────────────────

/// Before the fix, the shim sent flat `{title, project_id, description, goal}`
/// and the server (`CreatePlanBody { plan, external_ref }`) replied
/// `HTTP 422 missing field "plan"`. Now the shim wraps the body and supplies
/// a default `owner: {kind: "user"}` so creation succeeds.
#[tokio::test]
async fn plan_create_via_mcp_returns_success() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let pid = create_project_via_mcp(&client, "Demo Project").await;
    let resp = create_plan_via_mcp(&client, &pid, "Sprint 1").await;

    assert_eq!(
        resp["success"], true,
        "daruma_plan_create must succeed: {resp}"
    );
    assert!(
        resp["event_id"].is_string(),
        "event_id must be present: {resp}"
    );
    assert!(
        resp["event_seq"].is_number(),
        "event_seq must be present: {resp}"
    );
    assert!(
        resp["plan_id"].is_string(),
        "plan_id must be surfaced for agents: {resp}"
    );
    assert_eq!(
        resp["data"]["plan_id"], resp["plan_id"],
        "plan_id should be available both top-level and in data: {resp}"
    );
}

/// Optional plan fields (`description`, `goal`, `success_criteria`,
/// `parent_plan_id`) must flow into the wrapped `plan` object.
#[tokio::test]
async fn plan_create_via_mcp_accepts_optional_fields() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let pid = create_project_via_mcp(&client, "Demo Project").await;
    let resp = call_tool(
        &client,
        "daruma_plan_create",
        json!({
            "title": "Q1 Plan",
            "project_id": pid,
            "description": "Quarterly outcomes",
            "goal": "Ship §3.2",
            "success_criteria": ["12 AC green", "PR merged"],
        }),
    )
    .await;
    assert_eq!(resp["success"], true, "plan_create must succeed: {resp}");

    let plan_id = first_plan_id(&client, &pid).await;
    let summary = call_tool(&client, "daruma_plan_get", json!({ "id": plan_id })).await;
    assert_eq!(summary["plan"]["id"], plan_id, "compact plan id: {summary}");
    assert_eq!(
        summary["plan"]["title"], "Q1 Plan",
        "compact title: {summary}"
    );
    assert_eq!(
        summary["plan"]["status"], "draft",
        "compact status: {summary}"
    );
    assert_eq!(
        summary["plan"]["project_id"], pid,
        "compact project: {summary}"
    );
    assert!(
        summary["progress"].is_object(),
        "progress view must include counts: {summary}"
    );
    assert!(
        summary["plan"].get("goal").is_none(),
        "progress view must not include heavy plan fields: {summary}"
    );

    let plan = call_tool(
        &client,
        "daruma_plan_get",
        json!({ "id": plan_id, "view": "detail" }),
    )
    .await;
    assert_eq!(
        plan["plan"]["title"], "Q1 Plan",
        "title round-trips: {plan}"
    );
    assert_eq!(
        plan["plan"]["description"], "Quarterly outcomes",
        "description round-trips: {plan}"
    );
    assert_eq!(
        plan["plan"]["goal"], "Ship §3.2",
        "goal round-trips: {plan}"
    );
    assert_eq!(
        plan["plan"]["success_criteria"].as_array().unwrap().len(),
        2,
        "success_criteria round-trips: {plan}"
    );
}

// ── Regression: plan_update patch wrapper bug ─────────────────────────────────

/// Before the fix, the shim sent the raw patch object as the body and the
/// server (`UpdatePlanBody { patch }`) replied `HTTP 422 missing field
/// "patch"`. Now the shim wraps it.
#[tokio::test]
async fn plan_update_via_mcp_patches_fields() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let pid = create_project_via_mcp(&client, "Demo Project").await;
    create_plan_via_mcp(&client, &pid, "Plan A").await;
    let plan_id = first_plan_id(&client, &pid).await;

    let resp = call_tool(
        &client,
        "daruma_plan_update",
        json!({
            "id": plan_id,
            "patch": { "title": "Plan A (renamed)", "goal": "new goal" },
        }),
    )
    .await;
    assert_eq!(resp["success"], true, "plan_update must succeed: {resp}");

    let plan = call_tool(
        &client,
        "daruma_plan_get",
        json!({ "id": plan_id, "view": "detail" }),
    )
    .await;
    assert_eq!(
        plan["plan"]["title"], "Plan A (renamed)",
        "title must be patched: {plan}"
    );
    assert_eq!(
        plan["plan"]["goal"], "new goal",
        "goal must be patched: {plan}"
    );
}

/// The `docs/ARCHITECTURE.md` reparent example: `parent_plan_id` travels
/// inside `patch`. The flat form (`parent_plan_id` next to `id`, no `patch`)
/// is rejected and leaves the plan unchanged.
#[tokio::test]
async fn plan_update_reparents_only_via_patch_via_mcp() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let pid = create_project_via_mcp(&client, "Reparent Project").await;
    let parent_id = create_plan_via_mcp(&client, &pid, "Parent").await["plan_id"]
        .as_str()
        .expect("plan_create must return plan_id")
        .to_owned();
    let child_id = create_plan_via_mcp(&client, &pid, "Child").await["plan_id"]
        .as_str()
        .expect("plan_create must return plan_id")
        .to_owned();
    assert_ne!(parent_id, child_id);

    let parent_of = |plan: &serde_json::Value| plan["plan"]["parent_plan_id"].clone();

    let flat = dispatch_request(
        &client,
        req(
            "tools/call",
            json!({
                "name": "daruma_plan_update",
                "arguments": { "id": child_id, "parent_plan_id": parent_id },
            }),
        ),
    )
    .await
    .unwrap();
    let flat_failed =
        flat.error.is_some() || flat.result.as_ref().map(|r| r["isError"] == true) == Some(true);
    println!(
        "TRACE flat form -> error={:?} result={:?}",
        flat.error, flat.result
    );
    assert!(flat_failed, "flat parent_plan_id must be rejected");
    let plan = call_tool(
        &client,
        "daruma_plan_get",
        json!({ "id": child_id, "view": "detail" }),
    )
    .await;
    assert!(
        parent_of(&plan).is_null(),
        "flat form must not reparent: {plan}"
    );

    let resp = call_tool(
        &client,
        "daruma_plan_update",
        json!({ "id": child_id, "patch": { "parent_plan_id": parent_id } }),
    )
    .await;
    println!("TRACE patch form -> {resp}");
    assert_eq!(resp["success"], true, "patch form must succeed: {resp}");
    let plan = call_tool(
        &client,
        "daruma_plan_get",
        json!({ "id": child_id, "view": "detail" }),
    )
    .await;
    assert_eq!(
        parent_of(&plan),
        json!(parent_id),
        "patch form must reparent: {plan}"
    );

    let resp = call_tool(
        &client,
        "daruma_plan_update",
        json!({ "id": child_id, "patch": { "parent_plan_id": null } }),
    )
    .await;
    println!("TRACE unparent -> {resp}");
    assert_eq!(resp["success"], true, "null must unparent: {resp}");
    let plan = call_tool(
        &client,
        "daruma_plan_get",
        json!({ "id": child_id, "view": "detail" }),
    )
    .await;
    assert!(parent_of(&plan).is_null(), "null must drop to root: {plan}");
}

// ── Smoke: list + get + add_task + remove_task + reorder via MCP ──────────────

/// Quick smoke for the rest of the plan tool surface — exercises
/// `plan_list`, `plan_get`, `plan_add_task`, `plan_remove_task`,
/// `plan_reorder`, `plan_archive`. These already wrap bodies correctly
/// today; this test guards against future regressions.
#[tokio::test]
async fn plan_tool_surface_smoke() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let pid = create_project_via_mcp(&client, "Demo Project").await;
    create_plan_via_mcp(&client, &pid, "Smoke Plan").await;
    let plan_id = first_plan_id(&client, &pid).await;

    // Create two tasks and attach them to the plan.
    let t1 = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({ "plan": { "title": "seed plan", "project_id": pid }, "tasks": [ { "title": "Task 1" } ] }),
    )
    .await;
    let t1_id = t1["data"]
        .as_array()
        .and_then(|a| {
            a.iter().find_map(|e| {
                let p = e.get("payload")?;
                if p.get("type")?.as_str()? == "task_created" {
                    p.get("task")?.get("id")?.as_str().map(str::to_owned)
                } else {
                    None
                }
            })
        })
        .expect("task_created with id");

    let t2 = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({ "plan": { "title": "seed plan", "project_id": pid }, "tasks": [ { "title": "Task 2" } ] }),
    )
    .await;
    let t2_id = t2["data"]
        .as_array()
        .and_then(|a| {
            a.iter().find_map(|e| {
                let p = e.get("payload")?;
                if p.get("type")?.as_str()? == "task_created" {
                    p.get("task")?.get("id")?.as_str().map(str::to_owned)
                } else {
                    None
                }
            })
        })
        .expect("task_created with id");

    call_tool(
        &client,
        "daruma_plan_add_task",
        json!({ "plan_id": plan_id, "task_id": t1_id }),
    )
    .await;
    call_tool(
        &client,
        "daruma_plan_add_task",
        json!({ "plan_id": plan_id, "task_id": t2_id }),
    )
    .await;

    // Reorder: put t2 before t1.
    let reorder = call_tool(
        &client,
        "daruma_plan_reorder",
        json!({ "plan_id": plan_id, "order": [t2_id.clone(), t1_id.clone()] }),
    )
    .await;
    assert_eq!(reorder["success"], true, "reorder must succeed: {reorder}");

    // Remove t1.
    let remove = call_tool(
        &client,
        "daruma_plan_remove_task",
        json!({ "plan_id": plan_id, "task_id": t1_id }),
    )
    .await;
    assert_eq!(remove["success"], true, "remove must succeed: {remove}");

    // Archive.
    let archive = call_tool(&client, "daruma_plan_archive", json!({ "id": plan_id })).await;
    assert_eq!(archive["success"], true, "archive must succeed: {archive}");
}

// ── S2: removing a task that is a run's open step supersedes that step ──────
//
// Before S2 `current_step_task` was always `None` on the SQLite projection,
// so `plan_remove_task` never noticed the open step and left it dangling.
#[tokio::test]
async fn plan_remove_task_supersedes_open_run_step_via_mcp() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token);

    let pid = create_project_via_mcp(&client, "Step Project").await;
    let materialized = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({ "plan": { "title": "Step Plan", "project_id": pid }, "tasks": [ { "title": "Step task" } ] }),
    )
    .await;
    let created = |kind: &str, field: &str| -> String {
        materialized["data"]
            .as_array()
            .and_then(|a| {
                a.iter().find_map(|e| {
                    let p = e.get("payload")?;
                    (p.get("type")?.as_str()? == kind)
                        .then(|| p.get(field)?.get("id")?.as_str().map(str::to_owned))?
                })
            })
            .unwrap_or_else(|| panic!("{kind} in {materialized}"))
    };
    let plan_id = created("plan_created", "plan");
    let task_id = created("task_created", "task");
    let active = call_tool(
        &client,
        "daruma_plan_set_status",
        json!({ "plan_id": plan_id, "status": "active" }),
    )
    .await;
    assert_eq!(active["success"], true, "activate: {active}");

    let run = call_tool(
        &client,
        "daruma_run_start",
        json!({ "plan_id": plan_id, "agent_id": app.admin_agent_id.as_uuid().to_string() }),
    )
    .await;
    let run_id = run["run_id"].as_str().expect("run_id").to_owned();
    call_tool(
        &client,
        "daruma_run_start_step",
        json!({ "run_id": run_id, "task_id": task_id }),
    )
    .await;

    let remove = call_tool(
        &client,
        "daruma_plan_remove_task",
        json!({ "plan_id": plan_id, "task_id": task_id }),
    )
    .await;
    assert_eq!(remove["success"], true, "remove: {remove}");
    let timeline = client
        .get_json(&format!("/v1/runs/{run_id}/timeline"))
        .await
        .unwrap();
    let steps = timeline["steps"].as_array().expect("steps in timeline");
    assert_eq!(steps.len(), 1, "{timeline}");
    assert!(steps[0]["finished_at"].is_string(), "{timeline}");
    assert_eq!(steps[0]["outcome"]["kind"], "superseded", "{timeline}");
}

// ── S3: run_complete checks the pass postconditions ─────────────────────────
//
// Reproduces 01a126e4 over MCP: the step is finished `done` while the task
// never reached its handoff, so run_complete is refused and the run stays
// active; after the handoff transition the same call succeeds.
#[tokio::test]
async fn run_complete_requires_step_handoff_via_mcp() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token);

    let pid = create_project_via_mcp(&client, "Postcondition Project").await;
    let materialized = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({ "plan": { "title": "Postcondition Plan", "project_id": pid }, "tasks": [ { "title": "Builder task" } ] }),
    )
    .await;
    let created = |kind: &str, field: &str| -> String {
        materialized["data"]
            .as_array()
            .and_then(|a| {
                a.iter().find_map(|e| {
                    let p = e.get("payload")?;
                    (p.get("type")?.as_str()? == kind)
                        .then(|| p.get(field)?.get("id")?.as_str().map(str::to_owned))?
                })
            })
            .unwrap_or_else(|| panic!("{kind} in {materialized}"))
    };
    let plan_id = created("plan_created", "plan");
    let task_id = created("task_created", "task");
    let active = call_tool(
        &client,
        "daruma_plan_set_status",
        json!({ "plan_id": plan_id, "status": "active" }),
    )
    .await;
    assert_eq!(active["success"], true, "activate: {active}");

    let run = call_tool(
        &client,
        "daruma_run_start",
        json!({ "plan_id": plan_id, "agent_id": app.admin_agent_id.as_uuid().to_string() }),
    )
    .await;
    let run_id = run["run_id"].as_str().expect("run_id").to_owned();
    call_tool(
        &client,
        "daruma_run_start_step",
        json!({ "run_id": run_id, "task_id": task_id }),
    )
    .await;
    call_tool(
        &client,
        "daruma_run_finish_step",
        json!({ "run_id": run_id, "task_id": task_id, "outcome": { "kind": "done" } }),
    )
    .await;

    let refused = dispatch_request(
        &client,
        req(
            "tools/call",
            json!({ "name": "daruma_run_complete", "arguments": { "run_id": run_id } }),
        ),
    )
    .await
    .unwrap();
    let refused = format!("{refused:?}");
    assert!(refused.contains("run_postcondition_unmet"), "{refused}");
    assert!(refused.contains(&task_id), "{refused}");
    let timeline = client
        .get_json(&format!("/v1/runs/{run_id}/timeline"))
        .await
        .unwrap();
    assert_eq!(timeline["run"]["status"], "active", "{timeline}");

    let handoff = call_tool(
        &client,
        "daruma_set_status",
        json!({ "id": task_id, "status": "in_review" }),
    )
    .await;
    assert_eq!(handoff["success"], true, "handoff: {handoff}");
    let completed = call_tool(&client, "daruma_run_complete", json!({ "run_id": run_id })).await;
    assert_eq!(completed["success"], true, "complete: {completed}");
}

async fn active_one_task_plan_via_mcp(client: &ApiClient, title: &str) -> (String, String) {
    let pid = create_project_via_mcp(client, title).await;
    let materialized = call_tool(
        client,
        "daruma_plan_materialize",
        json!({ "plan": { "title": title, "project_id": pid }, "tasks": [ { "title": "Loop task" } ] }),
    )
    .await;
    let created = |kind: &str, field: &str| -> String {
        materialized["data"]
            .as_array()
            .and_then(|a| {
                a.iter().find_map(|e| {
                    let p = e.get("payload")?;
                    (p.get("type")?.as_str()? == kind)
                        .then(|| p.get(field)?.get("id")?.as_str().map(str::to_owned))?
                })
            })
            .unwrap_or_else(|| panic!("{kind} in {materialized}"))
    };
    let plan_id = created("plan_created", "plan");
    let task_id = created("task_created", "task");
    let active = call_tool(
        client,
        "daruma_plan_set_status",
        json!({ "plan_id": plan_id, "status": "active" }),
    )
    .await;
    assert_eq!(active["success"], true, "activate: {active}");
    (plan_id, task_id)
}

// docs/mcp/EXECUTOR-LOOP.md «MCP tool sequence», step by step through the
// MCP surface: the documented loop (run_start_step before the work) completes
// its run; the former order (complete, then run_finish_step with no started
// step) is refused by run_complete (01a144fe-e537-7bb0-8e87-9c66985bf9a4).
#[tokio::test]
async fn executor_loop_doc_sequence_completes_run_via_mcp() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token);
    let agent_id = app.admin_agent_id.as_uuid().to_string();

    let (plan_id, task_id) = active_one_task_plan_via_mcp(&client, "Executor Loop Plan").await;
    let run = call_tool(
        &client,
        "daruma_run_start",
        json!({ "plan_id": plan_id, "agent_id": agent_id }),
    )
    .await;
    let run_id = run["run_id"].as_str().expect("run_id").to_owned();
    let progress = call_tool(
        &client,
        "daruma_plan_progress",
        json!({ "plan_id": plan_id }),
    )
    .await;
    eprintln!("step 2 progress: {progress}");
    let next = call_tool(
        &client,
        "daruma_plan_next_task",
        json!({ "id": plan_id, "run_id": run_id, "claim_ttl_secs": 300 }),
    )
    .await;
    eprintln!("step 3 next: {next}");
    assert_eq!(next["task_id"], task_id.as_str(), "next: {next}");
    let step = call_tool(
        &client,
        "daruma_run_start_step",
        json!({ "run_id": run_id, "task_id": task_id }),
    )
    .await;
    assert_eq!(step["success"], true, "start_step: {step}");
    let intent = call_tool(
        &client,
        "daruma_set_status",
        json!({ "id": task_id, "status": "in_progress", "comment": { "body": "intent: loop scenario", "kind": "intent" } }),
    )
    .await;
    eprintln!("step 5 set_status: {intent}");
    let done = call_tool(
        &client,
        "daruma_complete",
        json!({ "id": task_id, "reason": "loop scenario", "result_summary": "doc sequence" }),
    )
    .await;
    eprintln!("step 7 complete: {done}");
    let finished = call_tool(
        &client,
        "daruma_run_finish_step",
        json!({ "run_id": run_id, "task_id": task_id, "outcome": { "kind": "done" } }),
    )
    .await;
    assert_eq!(finished["success"], true, "finish_step: {finished}");
    let progress = call_tool(
        &client,
        "daruma_plan_progress",
        json!({ "plan_id": plan_id }),
    )
    .await;
    eprintln!("step 2 again progress: {progress}");
    assert!(progress["next_ready"].is_null(), "drained: {progress}");
    let completed = call_tool(&client, "daruma_run_complete", json!({ "run_id": run_id })).await;
    assert_eq!(completed["success"], true, "complete: {completed}");
    let timeline = client
        .get_json(&format!("/v1/runs/{run_id}/timeline"))
        .await
        .unwrap();
    eprintln!("documented loop run {run_id} timeline: {timeline}");
    assert_eq!(timeline["run"]["status"], "completed", "{timeline}");

    let (plan_id, task_id) = active_one_task_plan_via_mcp(&client, "Former Loop Plan").await;
    let run = call_tool(
        &client,
        "daruma_run_start",
        json!({ "plan_id": plan_id, "agent_id": agent_id }),
    )
    .await;
    let run_id = run["run_id"].as_str().expect("run_id").to_owned();
    call_tool(
        &client,
        "daruma_plan_next_task",
        json!({ "id": plan_id, "run_id": run_id, "claim_ttl_secs": 300 }),
    )
    .await;
    call_tool(
        &client,
        "daruma_complete",
        json!({ "id": task_id, "reason": "loop scenario", "result_summary": "former order" }),
    )
    .await;
    call_tool(
        &client,
        "daruma_run_finish_step",
        json!({ "run_id": run_id, "task_id": task_id, "outcome": { "kind": "done" } }),
    )
    .await;
    let refused = dispatch_request(
        &client,
        req(
            "tools/call",
            json!({ "name": "daruma_run_complete", "arguments": { "run_id": run_id } }),
        ),
    )
    .await
    .unwrap();
    let refused = format!("{refused:?}");
    eprintln!("former loop run {run_id} run_complete: {refused}");
    assert!(refused.contains("run_postcondition_unmet"), "{refused}");
    assert!(refused.contains(&task_id), "{refused}");
    let timeline = client
        .get_json(&format!("/v1/runs/{run_id}/timeline"))
        .await
        .unwrap();
    assert_eq!(timeline["run"]["status"], "active", "{timeline}");
}

#[tokio::test]
async fn plan_status_override_reaches_http_and_returns_warning_via_mcp() {
    let app = TestAppBuilder::default()
        .lifecycle_gate(Arc::new(PlanOverrideGate))
        .build()
        .await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token);
    let project_id = create_project_via_mcp(&client, "Override Project").await;
    create_plan_via_mcp(&client, &project_id, "Override Plan").await;
    let plan_id = first_plan_id(&client, &project_id).await;

    let response = call_tool(
        &client,
        "daruma_plan_set_status",
        json!({
            "plan_id": plan_id,
            "status": "active",
            "force": true,
            "override_reason": "urgent production repair"
        }),
    )
    .await;
    assert_eq!(response["success"], true);
    assert_eq!(
        response["warnings"][0]["code"], "rule_warning:plan-approval-proof",
        "response: {response}"
    );
}

// ── §3.8.14 plan_next_task sort order verification (CTM A.7) ──────────────────
//
// Locks in the documented resolver semantics:
//   1. Active-plan gate: Draft → returns null.
//   2. Eligible-set := plan_tasks where task.status != Done AND every dep is Done.
//   3. Order: position ASC among the eligible set (no priority key, no dep-count
//      key — both were considered in the ROADMAP brief and explicitly rejected
//      in favour of strict positional order).
//   4. Empty eligible-set → null.
//
// Construction: t1..t4 attached at positions 0..3, with t2 (pos 1) depending on
// t3 (pos 2). That layout forces the resolver to *skip* the lower-position task
// while its dep is unmet, and to come back to it after the dep resolves — i.e.
// position alone cannot explain the trace, dep filtering must be exercised.
#[tokio::test]
async fn plan_next_task_orders_by_position_and_skips_blocked() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let pid = create_project_via_mcp(&client, "Demo Project").await;
    create_plan_via_mcp(&client, &pid, "Order Plan").await;
    let plan_id = first_plan_id(&client, &pid).await;

    // Helper: create a task and pull its id out of the dispatch envelope.
    async fn make_task(client: &ApiClient, pid: &str, title: &str) -> String {
        let resp = call_tool(
            client,
            "daruma_plan_materialize",
            json!({ "plan": { "title": "seed plan", "project_id": pid }, "tasks": [ { "title": title } ] }),
        )
        .await;
        resp["data"]
            .as_array()
            .and_then(|a| {
                a.iter().find_map(|e| {
                    let p = e.get("payload")?;
                    if p.get("type")?.as_str()? == "task_created" {
                        p.get("task")?.get("id")?.as_str().map(str::to_owned)
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| panic!("task_created with id for {title}"))
    }

    let t1 = make_task(&client, &pid, "T1").await;
    let t2 = make_task(&client, &pid, "T2").await;
    let t3 = make_task(&client, &pid, "T3").await;
    let t4 = make_task(&client, &pid, "T4").await;

    // Attach in positional order. Only t2 has an explicit dep on t3.
    for (task_id, deps) in [
        (&t1, vec![]),
        (&t2, vec![t3.clone()]),
        (&t3, vec![]),
        (&t4, vec![]),
    ] {
        let mut args = json!({ "plan_id": plan_id, "task_id": task_id });
        if !deps.is_empty() {
            args["depends_on"] = json!(deps);
        }
        let resp = call_tool(&client, "daruma_plan_add_task", args).await;
        assert_eq!(resp["success"], true, "plan_add_task must succeed: {resp}");
    }

    // Resolver returns null while plan is Draft (status gate).
    let run_id = uuid::Uuid::now_v7().to_string();
    let draft_resp = call_tool(
        &client,
        "daruma_plan_next_task",
        json!({ "id": plan_id, "run_id": run_id }),
    )
    .await;
    assert!(
        draft_resp.is_null(),
        "next_task on Draft plan must be null, got: {draft_resp}"
    );

    // Activate the plan.
    let activate = call_tool(
        &client,
        "daruma_plan_set_status",
        json!({ "plan_id": plan_id, "status": "active" }),
    )
    .await;
    assert_eq!(
        activate["success"], true,
        "activate must succeed: {activate}"
    );

    // next_task helper: returns the task_id string, or None when null.
    async fn next_task(client: &ApiClient, plan_id: &str) -> Option<String> {
        let resp = call_tool(
            client,
            "daruma_plan_next_task",
            json!({ "id": plan_id, "run_id": uuid::Uuid::now_v7().to_string() }),
        )
        .await;
        if resp.is_null() {
            None
        } else {
            Some(resp["task_id"].as_str().unwrap().to_owned())
        }
    }

    // ttl_secs=0 is the compatibility contract for a pure peek: it must not
    // manufacture an expiry or acquire a claim.
    let zero_ttl = call_tool(
        &client,
        "daruma_plan_next_task",
        json!({
            "id": plan_id,
            "run_id": uuid::Uuid::now_v7().to_string(),
            "claim_ttl_secs": 0
        }),
    )
    .await;
    assert_eq!(zero_ttl["task_id"], t1);
    assert_eq!(zero_ttl["claim_expires_at"], serde_json::Value::Null);
    let claims = client.get_json("/v1/claims").await.unwrap();
    assert!(claims["claims"].as_array().unwrap().is_empty());

    // Step 1: t1 is at position 0 with no deps → wins.
    assert_eq!(
        next_task(&client, &plan_id).await.as_deref(),
        Some(t1.as_str()),
        "position 0 with no deps must be returned first"
    );

    // Complete t1. Now eligible-set = {t3, t4} (t2 is blocked by t3).
    // Position-asc → t3 (pos 2) before t4 (pos 3). Critically, t2 (pos 1) is
    // SKIPPED despite having a lower position than t3.
    let c1 = call_tool(&client, "daruma_complete", json!({ "id": t1 })).await;
    assert_eq!(c1["success"], true, "complete t1 must succeed: {c1}");
    assert_eq!(
        next_task(&client, &plan_id).await.as_deref(),
        Some(t3.as_str()),
        "blocked task at pos 1 must be skipped; pos 2 must win over pos 3"
    );

    // Complete t3 → t2 unblocks. Eligible-set = {t2, t4}; t2 (pos 1) wins.
    let c3 = call_tool(&client, "daruma_complete", json!({ "id": t3 })).await;
    assert_eq!(c3["success"], true, "complete t3 must succeed: {c3}");
    assert_eq!(
        next_task(&client, &plan_id).await.as_deref(),
        Some(t2.as_str()),
        "after dep is met, lower-position task must win"
    );

    // Complete t2 → only t4 left.
    let c2 = call_tool(&client, "daruma_complete", json!({ "id": t2 })).await;
    assert_eq!(c2["success"], true, "complete t2 must succeed: {c2}");
    assert_eq!(
        next_task(&client, &plan_id).await.as_deref(),
        Some(t4.as_str()),
        "last remaining task must be returned"
    );

    // Complete t4 → resolver returns null.
    let c4 = call_tool(&client, "daruma_complete", json!({ "id": t4 })).await;
    assert_eq!(c4["success"], true, "complete t4 must succeed: {c4}");
    assert!(
        next_task(&client, &plan_id).await.is_none(),
        "empty eligible-set must return null"
    );
}

/// All protocol-level tests drive the complete catalogue explicitly; the
/// compact `default` profile has its own dedicated coverage in
/// `mcp_dispatch.rs::profiles`.
async fn dispatch_request(
    client: &ApiClient,
    req: JsonRpcRequest,
) -> Option<daruma_mcp::JsonRpcResponse> {
    dispatch_request_with_profile(client, ToolProfile::Full, req).await
}
