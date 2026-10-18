//! Task 01a14ae6…d802 — an agent closes a drained task the way the old
//! `daruma-next` prompt told it to, `daruma_complete … comment="<summary>"`,
//! against a real inline server. The work summary must reach the durable
//! `task_completed` event as `completion_note.result_summary` instead of
//! being dropped; a conflicting `result_summary` must fail before any write;
//! and the served prompt now names `result_summary`.

use daruma_mcp::{dispatch_request_with_profile, ApiClient, JsonRpcRequest, ToolProfile};
use serde_json::{json, Value};

mod common;
use common::{spawn_server, test_app};

async fn dispatch(
    client: &ApiClient,
    method: &str,
    params: Value,
) -> daruma_mcp::JsonRpcResponse {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: method.into(),
        params: Some(params),
    };
    dispatch_request_with_profile(client, ToolProfile::Full, req)
        .await
        .unwrap()
}

async fn call_tool(client: &ApiClient, name: &str, arguments: Value) -> Value {
    let resp = dispatch(
        client,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
    .await;
    assert!(resp.error.is_none(), "tool {name} failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    assert_ne!(result["isError"], true, "tool {name} failed: {result}");
    let text = result["content"][0]["text"].as_str().unwrap().to_owned();
    serde_json::from_str(&text).unwrap()
}

async fn completions_of(client: &ApiClient, task_id: &str) -> Vec<Value> {
    let events = call_tool(client, "daruma_events_since", json!({ "limit": 1000 })).await;
    let list = events["data"]
        .as_array()
        .or_else(|| events.as_array())
        .cloned()
        .unwrap_or_else(|| panic!("events: {events}"));
    list.into_iter()
        .filter_map(|e| {
            let p = e.get("payload")?.clone();
            (p.get("type")?.as_str()? == "task_completed"
                && p.get("task_id")?.as_str()?.ends_with(task_id))
            .then_some(p)
        })
        .collect()
}

#[tokio::test]
async fn agent_closing_with_comment_keeps_its_work_summary() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token);

    let prompt = dispatch(&client, "prompts/get", json!({ "name": "daruma-next" })).await;
    let prompt = prompt.result.expect("prompt").to_string();
    assert!(
        prompt.contains("daruma_complete task_id=<task_id> [result_summary="),
        "{prompt}"
    );
    assert!(!prompt.contains("[comment="), "{prompt}");

    let project = call_tool(
        &client,
        "daruma_project_create",
        json!({ "title": "Summary alias" }),
    )
    .await;
    let pid = project["project_id"].as_str().unwrap().to_owned();
    let materialized = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({ "plan": { "title": "Close with comment", "project_id": pid },
                "tasks": [ { "title": "Ship the fix" } ] }),
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
    call_tool(
        &client,
        "daruma_plan_set_status",
        json!({ "plan_id": plan_id, "status": "active" }),
    )
    .await;

    let drained = call_tool(
        &client,
        "daruma_plan_drain_next",
        json!({ "plan_id": plan_id, "claim_ttl_secs": 600 }),
    )
    .await;
    assert_eq!(drained["task_id"], task_id.as_str(), "drain: {drained}");
    let run_id = drained["run_id"].as_str().expect("run_id").to_owned();
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

    // Two different summaries: rejected before any request, task stays open.
    let conflict = dispatch(
        &client,
        "tools/call",
        json!({ "name": "daruma_complete", "arguments": {
            "task_id": task_id, "result_summary": "one", "comment": "two" } }),
    )
    .await;
    let conflict = format!("{conflict:?}");
    assert!(conflict.contains("conflicting completion summaries"), "{conflict}");
    assert!(completions_of(&client, &task_id).await.is_empty());
    let open = call_tool(&client, "daruma_get", json!({ "id": task_id })).await;
    assert_ne!(open["status"], "done", "{open}");
    println!("TRACE conflict rejected, task status={}", open["status"]);

    let summary = "Fixed the dropped summary; 5 focused tests green.";
    let closed = call_tool(
        &client,
        "daruma_complete",
        json!({ "task_id": task_id, "comment": summary }),
    )
    .await;
    assert_eq!(closed["success"], true, "complete: {closed}");
    call_tool(&client, "daruma_run_complete", json!({ "run_id": run_id })).await;

    let done = call_tool(&client, "daruma_get", json!({ "id": task_id })).await;
    assert_eq!(done["status"], "done", "{done}");
    let completions = completions_of(&client, &task_id).await;
    assert_eq!(completions.len(), 1, "{completions:?}");
    assert_eq!(
        completions[0]["completion_note"]["result_summary"], summary,
        "{completions:?}"
    );
    println!(
        "TRACE run_id={run_id} task_id={task_id} task_completed={}",
        completions[0]
    );
}
