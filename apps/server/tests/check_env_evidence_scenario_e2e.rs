//! Task 01a07af1-…-23e760cf0f5b — meisei.ru ADR-0016 phase 1 on a real inline
//! HTTP server, driven through MCP `tools/call`: the check environment rides
//! the evidence payload and a rule demands it only through `required_fields`.
//! The server is unchanged; this proves the existing presence check carries
//! the contract (ADR-0016 validation gate 6).
//!
//! Until the ADR-0010 `check_passed` kind exists, the keys ride the evidence
//! the task's rule asks for — here a task-scoped `completion_note`.
//!
//! Expected (stated before execution):
//! 1. Strict plan: the rule lists `runs`, `passed`, `env_kind`, `env_ref`;
//!    evidence with `runs`/`passed` only → `daruma_complete` is refused with
//!    the rule key, the task stays `in_progress`;
//! 2. the same check re-submitted with `env_kind: host`,
//!    `env_ref: <hostname>:<toolchain>` → complete succeeds, task `done`;
//! 3. Soft plan: the rule lists `runs`, `passed` only; evidence without the
//!    environment closes the task (a soft assertion, not a refusal).

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

async fn task_status(client: &ApiClient, task_id: &str) -> String {
    let task = call_tool(client, "daruma_get", json!({ "id": task_id })).await;
    task.pointer("/task/status")
        .or_else(|| task.get("status"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("status in {task}"))
        .to_owned()
}

/// One plan with one task and a check rule on its completion; returns
/// `(plan_id, task_id)` with the task drained to `in_progress`.
async fn plan_with_check_rule(
    client: &ApiClient,
    project_id: &str,
    title: &str,
    rule_key: &str,
    required_fields: &[&str],
) -> (String, String) {
    let plan = call_tool(
        client,
        "daruma_plan_materialize",
        json!({
            "plan": { "title": title, "project_id": project_id },
            "tasks": [ { "title": format!("{title}: прогнать cargo test") } ]
        }),
    )
    .await;
    let plan_id = created(&plan, "plan_created", "plan").remove(0);
    let task_id = created(&plan, "task_created", "task").remove(0);
    call_tool(
        client,
        "daruma_plan_set_status",
        json!({ "plan_id": plan_id, "status": "active" }),
    )
    .await;
    call_tool(
        client,
        "daruma_rule_create",
        json!({ "rule": {
            "rule_key": rule_key,
            "title": rule_key,
            "scope": { "kind": "plan", "id": plan_id },
            "trigger": "task.before_complete",
            "requirement": { "type": "completion_note", "required_fields": required_fields },
            "mode": "required",
            "message": format!("{rule_key}: проверка без названной среды — утверждение"),
            "override_allowed": true
        }}),
    )
    .await;
    let claimed = call_tool(
        client,
        "daruma_plan_drain_next",
        json!({ "plan_id": plan_id }),
    )
    .await;
    assert_eq!(claimed["task_id"], json!(task_id), "{claimed}");
    (plan_id, task_id)
}

async fn submit_check(client: &ApiClient, task_id: &str, payload: Value) {
    call_tool(
        client,
        "daruma_evidence_submit",
        json!({ "evidence": {
            "kind": "completion_note",
            "scope": { "kind": "task", "id": task_id },
            "reason": "cargo test -p corporate-game --test hud",
            "payload": payload
        }}),
    )
    .await;
}

#[tokio::test]
async fn corporate_game_strict_check_needs_its_environment() {
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

    // 1. Strict: a green check without its environment does not close the task.
    let (strict_plan, strict_task) = plan_with_check_rule(
        &client,
        &pid,
        "HUD: таймер (strict)",
        "check-env-strict",
        &["runs", "passed", "env_kind", "env_ref"],
    )
    .await;
    eprintln!("TRACE setup project={pid} strict_plan={strict_plan} strict_task={strict_task}");
    submit_check(&client, &strict_task, json!({ "runs": 1, "passed": 1 })).await;
    let err = tool_error(&client, "daruma_complete", json!({ "id": strict_task })).await;
    eprintln!("TRACE step1 complete error={err}");
    assert!(err.contains("check-env-strict"), "{err}");
    assert!(err.contains("`env_kind` is missing"), "{err}");
    assert_eq!(task_status(&client, &strict_task).await, "in_progress");

    // 2. The same check naming where it ran closes the task.
    submit_check(
        &client,
        &strict_task,
        json!({ "runs": 1, "passed": 1,
                "env_kind": "host", "env_ref": "av-wsl:rustc-1.89.0" }),
    )
    .await;
    let done = call_tool(&client, "daruma_complete", json!({ "id": strict_task })).await;
    eprintln!("TRACE step2 complete ok={done}");
    assert_eq!(task_status(&client, &strict_task).await, "done");
    let evidence = call_tool(
        &client,
        "daruma_evidence_list",
        json!({ "project_id": pid, "task_id": strict_task }),
    )
    .await;
    eprintln!("TRACE step2 evidence={evidence}");
    assert!(
        evidence.to_string().contains("av-wsl:rustc-1.89.0"),
        "env_ref kept in the record: {evidence}"
    );

    // 3. Soft: the rule does not list the environment; the record counts.
    let (soft_plan, soft_task) = plan_with_check_rule(
        &client,
        &pid,
        "HUD: пауза (soft)",
        "check-env-soft",
        &["runs", "passed"],
    )
    .await;
    eprintln!("TRACE step3 soft_plan={soft_plan} soft_task={soft_task}");
    submit_check(&client, &soft_task, json!({ "runs": 1, "passed": 1 })).await;
    call_tool(&client, "daruma_complete", json!({ "id": soft_task })).await;
    assert_eq!(task_status(&client, &soft_task).await, "done");
}
