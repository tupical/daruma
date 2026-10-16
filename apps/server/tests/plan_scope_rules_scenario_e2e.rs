//! Task 01a130a0 — corporate-game reproduction of the plan-scope rule bypass,
//! driven through MCP `tools/call` against a real inline HTTP server.
//!
//! Production: a plan-scoped `management-techdesign-read` rule (required,
//! non-overridable, `title_prefix` "Игровое время в HUD") did not stop a
//! direct start of a matching task. Here the rule sits on the PARENT plan and
//! the task lives in a CHILD plan — the shape a decomposed corporate-game
//! plan takes — and a second parent rule gates `task.created`, which
//! `daruma_plan_materialize` stages in one unpersisted batch.
//!
//! Expected (stated before execution):
//! 1. forced direct start of the child task → `isError`, rule
//!    `management-techdesign-read`; the task stays `todo`;
//! 2. `daruma_plan_drain_next` on the child plan claims nothing;
//! 3. appending a matching task to the child plan → `isError`, rule
//!    `management-intake-read`; the plan's task count is unchanged;
//! 4. after a `document_read_ack` of the tech design, drain claims the task
//!    (run id returned) and it is `in_progress`.

use daruma_mcp::{dispatch_request_with_profile, ApiClient, JsonRpcRequest, ToolProfile};
use serde_json::{json, Value};

mod common;
use common::{spawn_server, TestAppBuilder};

async fn dispatch(client: &ApiClient, name: &str, arguments: Value) -> Value {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: "tools/call".into(),
        params: Some(json!({ "name": name, "arguments": arguments })),
    };
    let resp = dispatch_request_with_profile(client, ToolProfile::Full, req)
        .await
        .unwrap();
    assert!(resp.error.is_none(), "tool {name}: {:?}", resp.error);
    resp.result.unwrap()
}

fn text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

async fn call_tool(client: &ApiClient, name: &str, arguments: Value) -> Value {
    let result = dispatch(client, name, arguments).await;
    assert_ne!(result["isError"], true, "tool {name} failed: {result}");
    serde_json::from_str(&text(&result)).unwrap()
}

/// A tool call that must fail; returns its error text.
async fn tool_error(client: &ApiClient, name: &str, arguments: Value) -> String {
    let result = dispatch(client, name, arguments).await;
    assert_eq!(result["isError"], true, "tool {name} must fail: {result}");
    text(&result)
}

fn created(materialized: &Value, kind: &str, field: &str) -> Vec<String> {
    materialized["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| {
            let p = e.get("payload")?;
            (p.get("type")?.as_str()? == kind)
                .then(|| p.get(field)?.get("id")?.as_str().map(str::to_owned))?
        })
        .collect()
}

fn read_rule(rule_key: &str, plan_id: &str, trigger: &str) -> Value {
    json!({ "rule": {
        "rule_key": rule_key,
        "title": rule_key,
        "scope": { "kind": "plan", "id": plan_id },
        "trigger": trigger,
        "condition": { "title_prefix": ["Игровое время в HUD"] },
        "requirement": { "type": "read_artifact", "doc_ref": "techdesign-hud", "min_version": "1" },
        "mode": "required",
        "message": format!("{rule_key}: прочитайте техдизайн HUD"),
        "override_allowed": false
    }})
}

async fn task_status(client: &ApiClient, task_id: &str) -> String {
    let task = call_tool(client, "daruma_get", json!({ "id": task_id })).await;
    task.pointer("/task/status")
        .or_else(|| task.get("status"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("status in {task}"))
        .to_owned()
}

#[tokio::test]
async fn corporate_game_parent_plan_rules_reach_child_plan_tasks() {
    let app = TestAppBuilder::default().rule_engine_gate().build().await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token.clone());

    let project = call_tool(
        &client,
        "daruma_project_create",
        json!({ "title": "Corporate game" }),
    )
    .await;
    let pid = project["project_id"].as_str().unwrap().to_owned();

    let parent = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": { "title": "Корпоративная игра: HUD", "project_id": pid },
            "tasks": [ { "title": "Игровое время в HUD: часы" } ]
        }),
    )
    .await;
    let parent_id = created(&parent, "plan_created", "plan").remove(0);
    let child = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": { "title": "HUD: таймер", "project_id": pid, "parent_plan_id": parent_id },
            "tasks": [ { "title": "Игровое время в HUD: таймер" } ]
        }),
    )
    .await;
    let child_id = created(&child, "plan_created", "plan").remove(0);
    let task_id = created(&child, "task_created", "task").remove(0);
    for plan in [&parent_id, &child_id] {
        call_tool(
            &client,
            "daruma_plan_set_status",
            json!({ "plan_id": plan, "status": "active" }),
        )
        .await;
    }
    for (key, trigger) in [
        ("management-techdesign-read", "task.before_start"),
        ("management-intake-read", "task.created"),
    ] {
        call_tool(
            &client,
            "daruma_rule_create",
            read_rule(key, &parent_id, trigger),
        )
        .await;
    }
    eprintln!(
        "TRACE setup project={pid} parent_plan={parent_id} child_plan={child_id} task={task_id}"
    );

    // 1. Forced direct start: the parent's non-overridable rule blocks it.
    let err = tool_error(
        &client,
        "daruma_set_status",
        json!({ "id": task_id, "status": "in_progress", "force": true,
                "override_reason": "срочно в релиз" }),
    )
    .await;
    eprintln!("TRACE step1 set_status error={err}");
    assert!(err.contains("management-techdesign-read"), "{err}");
    assert_eq!(task_status(&client, &task_id).await, "todo");

    // 2. Drain path hits the same gate and claims nothing.
    let drained = dispatch(
        &client,
        "daruma_plan_drain_next",
        json!({ "plan_id": child_id }),
    )
    .await;
    eprintln!("TRACE step2 drain_next result={}", text(&drained));
    assert_eq!(task_status(&client, &task_id).await, "todo");

    // 3. Appending a matching task is gated at creation, before persist.
    let before = call_tool(&client, "daruma_plan_get", json!({ "id": child_id })).await;
    let err = tool_error(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": { "id": child_id },
            "tasks": [ { "title": "Игровое время в HUD: пауза" } ]
        }),
    )
    .await;
    eprintln!("TRACE step3 materialize error={err}");
    assert!(err.contains("management-intake-read"), "{err}");
    let after = call_tool(&client, "daruma_plan_get", json!({ "id": child_id })).await;
    assert_eq!(
        before["progress"]["tasks_total"], after["progress"]["tasks_total"],
        "blocked append persisted a task: {after}"
    );

    // 4. Reading the tech design unblocks the inherited requirement.
    call_tool(
        &client,
        "daruma_evidence_submit",
        json!({ "evidence": {
            "kind": "document_read_ack",
            "scope": { "kind": "task", "id": task_id },
            "target": "techdesign-hud",
            "doc_version": "1",
            "reason": "прочитал техдизайн HUD v1"
        }}),
    )
    .await;
    let claimed = call_tool(
        &client,
        "daruma_plan_drain_next",
        json!({ "plan_id": child_id }),
    )
    .await;
    eprintln!("TRACE step4 drain_next claimed={claimed}");
    assert_eq!(claimed["task_id"], json!(task_id), "{claimed}");
    assert!(claimed["run_id"].is_string(), "{claimed}");
    assert_eq!(task_status(&client, &task_id).await, "in_progress");
}
