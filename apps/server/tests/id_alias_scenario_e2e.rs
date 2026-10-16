//! Plan 01a06cfc, task 04c37251 — an agent drives the whole plan loop
//! (activate → drain → run step → handoff → run complete) against a real
//! inline server while mixing up `id` / `task_id` / `plan_id` / `run_id`,
//! the mistake that used to fail with "`id` (string) is required" and
//! silently skip the mutation. A wrong-kind id fails before any write.

use daruma_mcp::{dispatch_request_with_profile, ApiClient, JsonRpcRequest, ToolProfile};
use serde_json::{json, Value};

mod common;
use common::{spawn_server, test_app};

async fn dispatch(client: &ApiClient, name: &str, arguments: Value) -> daruma_mcp::JsonRpcResponse {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: "tools/call".into(),
        params: Some(json!({ "name": name, "arguments": arguments })),
    };
    dispatch_request_with_profile(client, ToolProfile::Full, req)
        .await
        .unwrap()
}

async fn call_tool(client: &ApiClient, name: &str, arguments: Value) -> Value {
    let resp = dispatch(client, name, arguments).await;
    assert!(resp.error.is_none(), "tool {name} failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    assert_ne!(result["isError"], true, "tool {name} failed: {result}");
    let text = result["content"][0]["text"].as_str().unwrap().to_owned();
    serde_json::from_str(&text).unwrap()
}

async fn tool_error(client: &ApiClient, name: &str, arguments: Value) -> String {
    format!("{:?}", dispatch(client, name, arguments).await)
}

#[tokio::test]
async fn agent_loop_with_mixed_up_id_names_reaches_the_server() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token);

    let project = call_tool(
        &client,
        "daruma_project_create",
        json!({ "title": "Aliases" }),
    )
    .await;
    let pid = project["project_id"].as_str().unwrap().to_owned();
    let materialized = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({ "plan": { "title": "Alias plan", "project_id": pid }, "tasks": [ { "title": "Alias task" } ] }),
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

    // Schema name is `plan_id`; the agent sends `id`.
    let active = call_tool(
        &client,
        "daruma_plan_set_status",
        json!({ "id": plan_id, "status": "active" }),
    )
    .await;
    assert_eq!(active["success"], true, "activate: {active}");

    let drained = call_tool(
        &client,
        "daruma_plan_drain_next",
        json!({ "id": plan_id, "claim_ttl_secs": 600 }),
    )
    .await;
    assert_eq!(drained["task_id"], task_id.as_str(), "drain: {drained}");
    let run_id = drained["run_id"].as_str().expect("run_id").to_owned();

    // `id` fills whichever side of a two-id tool is missing.
    call_tool(
        &client,
        "daruma_run_start_step",
        json!({ "id": run_id, "task_id": task_id }),
    )
    .await;

    // A plan id where a task id belongs fails before any request.
    let wrong = tool_error(
        &client,
        "daruma_set_status",
        json!({ "id": format!("pln_{plan_id}"), "status": "done" }),
    )
    .await;
    assert!(
        wrong.contains("is a plan id, but this tool needs a task id"),
        "{wrong}"
    );

    call_tool(
        &client,
        "daruma_run_finish_step",
        json!({ "run_id": run_id, "id": task_id, "outcome": { "kind": "done" } }),
    )
    .await;
    let handoff = call_tool(
        &client,
        "daruma_set_status",
        json!({ "task_id": task_id, "status": "in_review" }),
    )
    .await;
    assert_eq!(handoff["success"], true, "handoff: {handoff}");
    let completed = call_tool(&client, "daruma_run_complete", json!({ "id": run_id })).await;
    assert_eq!(completed["success"], true, "complete: {completed}");

    let task = call_tool(&client, "daruma_get", json!({ "task_id": task_id })).await;
    assert_eq!(task["status"], "in_review", "{task}");
    let timeline = client
        .get_json(&format!("/v1/runs/{run_id}/timeline"))
        .await
        .unwrap();
    assert_eq!(timeline["run"]["status"], "completed", "{timeline}");
    println!("scenario run_id={run_id} task_id={task_id} timeline={timeline}");
}
