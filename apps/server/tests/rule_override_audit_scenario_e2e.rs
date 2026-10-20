//! Task 019fd222 — corporate-game scenario: an emergency bypass of a
//! mandatory rule leaves a trace, driven through MCP `tools/call` against a
//! real inline HTTP server.
//!
//! Before the fix a `force` + `override_reason` bypass landed in the event log
//! as `rule_fired { decision: "warning" }` — the same shape as an ignored
//! recommendation — and the reason was dropped.
//!
//! Setup: project "Corporate game", task "Игровое время в HUD: часы", a
//! required overridable `hud-completion-note` rule and a recommendation
//! `hud-impact-review` rule, both on `task.before_complete`.
//!
//! Expected (stated before execution):
//! 1. plain `done` → `isError` naming `hud-completion-note`; task stays `todo`;
//! 2. `done` with `force` + padded `override_reason` → success, both rules
//!    come back as warnings; task is `done`;
//! 3. `daruma_events_since` shows for `hud-completion-note` exactly one
//!    `blocked` (step 1) and one `overridden` carrying the trimmed reason, and
//!    no `warning`; `hud-impact-review` is a `warning` without a reason.

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

async fn task_status(client: &ApiClient, task_id: &str) -> String {
    let task = call_tool(client, "daruma_get", json!({ "id": task_id })).await;
    task.pointer("/task/status")
        .or_else(|| task.get("status"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("status in {task}"))
        .to_owned()
}

/// Every event payload of `kind` anywhere in a response, in order.
fn payloads<'a>(value: &'a Value, kind: &str, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            if let Some(payload) = map.get("payload") {
                if payload.get("type").and_then(Value::as_str) == Some(kind) {
                    out.push(payload);
                    return;
                }
            }
            map.values().for_each(|v| payloads(v, kind, out));
        }
        Value::Array(items) => items.iter().for_each(|v| payloads(v, kind, out)),
        _ => {}
    }
}

#[tokio::test]
async fn corporate_game_rule_override_is_audited_with_its_reason() {
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
    let plan = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": { "title": "Корпоративная игра: HUD", "project_id": pid },
            "tasks": [ { "title": "Игровое время в HUD: часы" } ]
        }),
    )
    .await;
    let mut tasks = Vec::new();
    payloads(&plan, "task_created", &mut tasks);
    let task_id = tasks[0]["task"]["id"].as_str().unwrap().to_owned();
    for (key, mode, requirement) in [
        (
            "hud-completion-note",
            "required",
            json!({ "type": "completion_note", "required_fields": [] }),
        ),
        (
            "hud-impact-review",
            "recommendation",
            json!({ "type": "impact_check", "target": "hud", "required_fields": ["risk_level"] }),
        ),
    ] {
        call_tool(
            &client,
            "daruma_rule_create",
            json!({ "rule": {
                "rule_key": key,
                "title": key,
                "scope": { "kind": "project", "id": pid },
                "trigger": "task.before_complete",
                "requirement": requirement,
                "mode": mode,
                "message": format!("{key}: оформите итог по часам HUD"),
                "override_allowed": true
            }}),
        )
        .await;
    }
    eprintln!("TRACE setup project={pid} task={task_id}");

    // 1. A normal completion is stopped by the required rule.
    let blocked = dispatch(
        &client,
        "daruma_set_status",
        json!({ "id": task_id, "status": "done" }),
    )
    .await;
    eprintln!("TRACE step1 set_status result={}", text(&blocked));
    assert_eq!(blocked["isError"], true, "{blocked}");
    assert!(text(&blocked).contains("hud-completion-note"), "{blocked}");
    assert_eq!(task_status(&client, &task_id).await, "todo");

    // 2. The emergency bypass passes; the executor still sees both rules.
    let forced = call_tool(
        &client,
        "daruma_set_status",
        json!({ "id": task_id, "status": "done", "force": true,
                "override_reason": "  срочно в релиз: демо у заказчика  " }),
    )
    .await;
    eprintln!("TRACE step2 set_status result={forced}");
    let warned = forced.to_string();
    assert!(
        warned.contains("rule_warning:hud-completion-note"),
        "{forced}"
    );
    assert!(
        warned.contains("rule_warning:hud-impact-review"),
        "{forced}"
    );
    assert_eq!(task_status(&client, &task_id).await, "done");

    // 3. The event log tells the bypass apart from the recommendation.
    let events = call_tool(
        &client,
        "daruma_events_since",
        json!({ "seq": 0, "limit": 1000 }),
    )
    .await;
    let mut fired = Vec::new();
    payloads(&events, "rule_fired", &mut fired);
    let audit: Vec<(String, String, Value)> = fired
        .iter()
        .map(|p| {
            (
                p["rule_key"].as_str().unwrap_or_default().to_owned(),
                p["decision"].as_str().unwrap_or_default().to_owned(),
                p.get("override_reason").cloned().unwrap_or(Value::Null),
            )
        })
        .collect();
    eprintln!("TRACE step3 rule_fired={audit:?}");
    let reason = json!("срочно в релиз: демо у заказчика");
    assert_eq!(
        audit
            .iter()
            .filter(|(key, _, _)| key == "hud-completion-note")
            .map(|(_, decision, reason)| (decision.as_str(), reason.clone()))
            .collect::<Vec<_>>(),
        [("blocked", Value::Null), ("overridden", reason)],
        "{audit:?}"
    );
    assert!(
        audit
            .iter()
            .filter(|(key, _, _)| key == "hud-impact-review")
            .all(|(_, decision, reason)| decision == "warning" && reason.is_null()),
        "{audit:?}"
    );
    assert!(
        audit.iter().any(|(key, _, _)| key == "hud-impact-review"),
        "{audit:?}"
    );
}
