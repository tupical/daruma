//! Task 01a14ae6-…-8feaf23659b0 — corporate-game reproduction of an unblock
//! hint that pointed at a plan which did not exist yet, driven through MCP
//! `tools/call` against a real inline HTTP server.
//!
//! A project-scoped `task.created` rule requires a decision record. Creating
//! a NEW plan with `daruma_plan_materialize` stages the plan and its tasks in
//! one batch; the gate rejects the batch, so the plan is never saved. The
//! `unblock` hint used to suggest that staged plan as evidence scope — pasting
//! it into `daruma_evidence_submit` then failed with "plan not found".
//!
//! Expected (stated before execution):
//! 1. materializing the new plan → `isError`, rule `game-task-decision`, and
//!    the hint's `evidence.scope` is the persisted project, not a plan;
//! 2. `daruma_evidence_submit` with the hint's `evidence` object pasted
//!    verbatim (plus `reason`) succeeds;
//! 3. the same materialize call now succeeds and creates the plan + task.

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

/// The `unblock` array a `rule_blocked` error ends with. The tool error is
/// `HTTP <status>: {"error":{"message": ...}}`; the hint closes the message.
fn unblock_hints(err: &str) -> Vec<Value> {
    let body: Value = err
        .find('{')
        .and_then(|at| serde_json::from_str(&err[at..]).ok())
        .unwrap_or_else(|| panic!("tool error must carry a JSON body: {err}"));
    let message = body["error"]["message"].as_str().unwrap_or_default();
    let (_, suffix) = message
        .rsplit_once(" | unblock: ")
        .unwrap_or_else(|| panic!("blocked error must carry an unblock hint: {err}"));
    serde_json::from_str(suffix).unwrap_or_else(|e| panic!("unblock JSON ({e}): {suffix}"))
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

#[tokio::test]
async fn corporate_game_new_plan_hint_names_a_persisted_scope() {
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
    call_tool(
        &client,
        "daruma_rule_create",
        json!({ "rule": {
            "rule_key": "game-task-decision",
            "title": "game-task-decision",
            "scope": { "kind": "project", "id": pid },
            "trigger": "task.created",
            "requirement": { "type": "decision_record" },
            "mode": "required",
            "message": "game-task-decision: зафиксируйте решение по игре",
            "override_allowed": false
        }}),
    )
    .await;
    let materialize = json!({
        "plan": { "title": "Корпоративная игра: сезон 2", "project_id": pid },
        "tasks": [ { "title": "Сезон 2: таблица лидеров" } ]
    });
    eprintln!("TRACE setup project={pid}");

    // 1. The new plan's batch is blocked; the hint names the project.
    let err = tool_error(&client, "daruma_plan_materialize", materialize.clone()).await;
    eprintln!("TRACE step1 materialize error={err}");
    assert!(err.contains("game-task-decision"), "{err}");
    let hints = unblock_hints(&err);
    assert_eq!(hints.len(), 1, "{hints:?}");
    let evidence = hints[0]["evidence"].clone();
    assert_eq!(
        evidence["scope"],
        json!({ "kind": "project", "id": pid }),
        "hint must name the persisted project, not the staged plan: {}",
        hints[0]
    );

    // 2. The hint pastes straight into evidence_submit.
    let mut submitted = evidence.clone();
    submitted["reason"] = json!("решение по сезону 2 принято на планёрке");
    let recorded = call_tool(
        &client,
        "daruma_evidence_submit",
        json!({ "evidence": submitted }),
    )
    .await;
    eprintln!("TRACE step2 evidence_submit result={recorded}");

    // 3. The retry passes the gate and persists the plan with its task.
    let done = call_tool(&client, "daruma_plan_materialize", materialize).await;
    let plans = created(&done, "plan_created", "plan");
    let tasks = created(&done, "task_created", "task");
    eprintln!("TRACE step3 materialize plans={plans:?} tasks={tasks:?}");
    assert_eq!(plans.len(), 1, "{done}");
    assert_eq!(tasks.len(), 1, "{done}");
}
