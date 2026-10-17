//! Task 01a14418-…-e90062739b5c — corporate-game scenario for plan-level
//! weakening of a project requirement, driven through MCP `tools/call`
//! against a real inline HTTP server.
//!
//! The project requires a playtest report to be read before any "Плейтест"
//! task starts (`required`, `override_allowed=true`). The prototype plan
//! relaxes it to a recommendation; the release plan keeps the project rule.
//! Spec §2.3: the plan override replaces the parent rule for its members —
//! it must not be unioned with the stale project requirement.
//!
//! Expected (stated before execution):
//! 1. a direct start of the prototype task succeeds, the task is
//!    `in_progress`, and the response carries the plan-level
//!    `rule_warning:playtest-report-read` instead of a block;
//! 2. a direct start of the release task → `isError`, rule
//!    `playtest-report-read`; the task stays `todo`;
//! 3. after a `document_read_ack` of the report, the release task starts.

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

fn playtest_rule(scope: Value, mode: &str) -> Value {
    json!({ "rule": {
        "rule_key": "playtest-report-read",
        "title": "playtest-report-read",
        "scope": scope,
        "trigger": "task.before_start",
        "condition": { "title_prefix": ["Плейтест"] },
        "requirement": { "type": "read_artifact", "doc_ref": "playtest-report", "min_version": "1" },
        "mode": mode,
        "message": "playtest-report-read: прочитайте отчёт плейтеста",
        "override_allowed": true
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

async fn materialize(client: &ApiClient, pid: &str, plan: &str, task: &str) -> (String, String) {
    let materialized = call_tool(
        client,
        "daruma_plan_materialize",
        json!({
            "plan": { "title": plan, "project_id": pid },
            "tasks": [ { "title": task } ]
        }),
    )
    .await;
    let plan_id = created(&materialized, "plan_created", "plan").remove(0);
    let task_id = created(&materialized, "task_created", "task").remove(0);
    call_tool(
        client,
        "daruma_plan_set_status",
        json!({ "plan_id": plan_id, "status": "active" }),
    )
    .await;
    (plan_id, task_id)
}

#[tokio::test]
async fn corporate_game_prototype_plan_relaxes_the_project_playtest_rule() {
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
    let (prototype, prototype_task) = materialize(
        &client,
        &pid,
        "Корпоративная игра: прототип",
        "Плейтест: прототип HUD",
    )
    .await;
    let (release, release_task) = materialize(
        &client,
        &pid,
        "Корпоративная игра: релиз",
        "Плейтест: релизная сборка",
    )
    .await;
    call_tool(
        &client,
        "daruma_rule_create",
        playtest_rule(json!({ "kind": "project", "id": pid }), "required"),
    )
    .await;
    call_tool(
        &client,
        "daruma_rule_create",
        playtest_rule(json!({ "kind": "plan", "id": prototype }), "recommendation"),
    )
    .await;
    eprintln!(
        "TRACE setup project={pid} prototype_plan={prototype} prototype_task={prototype_task} \
         release_plan={release} release_task={release_task}"
    );

    // 1. The prototype plan's recommendation replaces the project requirement.
    let started = dispatch(
        &client,
        "daruma_set_status",
        json!({ "id": prototype_task, "status": "in_progress" }),
    )
    .await;
    let started_text = text(&started);
    eprintln!("TRACE step1 set_status result={started_text}");
    assert_ne!(started["isError"], true, "{started_text}");
    assert!(
        started_text.contains("rule_warning:playtest-report-read"),
        "plan-level warning surfaced: {started_text}"
    );
    assert_eq!(task_status(&client, &prototype_task).await, "in_progress");

    // 2. The release plan has no override: the project requirement blocks.
    let err = tool_error(
        &client,
        "daruma_set_status",
        json!({ "id": release_task, "status": "in_progress" }),
    )
    .await;
    eprintln!("TRACE step2 set_status error={err}");
    assert!(err.contains("playtest-report-read"), "{err}");
    assert_eq!(task_status(&client, &release_task).await, "todo");

    // 3. Reading the report satisfies the project requirement.
    call_tool(
        &client,
        "daruma_evidence_submit",
        json!({ "evidence": {
            "kind": "document_read_ack",
            "scope": { "kind": "task", "id": release_task },
            "target": "playtest-report",
            "doc_version": "1",
            "reason": "прочитал отчёт плейтеста v1"
        }}),
    )
    .await;
    let started = call_tool(
        &client,
        "daruma_set_status",
        json!({ "id": release_task, "status": "in_progress" }),
    )
    .await;
    eprintln!("TRACE step3 set_status result={started}");
    assert_eq!(task_status(&client, &release_task).await, "in_progress");
}
