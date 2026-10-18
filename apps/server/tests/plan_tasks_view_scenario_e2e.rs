//! Task 01a14418-…-e9dff0bd9dfd — an agent runs `/plan` on a seven-task
//! sprint plan against a real inline server. The default `progress` view
//! caps each group at five open tasks and carries no priority; `view=tasks`
//! must list every task in plan order — done included — with priority and
//! the ids still blocking it.

use daruma_mcp::{dispatch_request_with_profile, ApiClient, JsonRpcRequest, ToolProfile};
use serde_json::{json, Value};

mod common;
use common::{spawn_server, test_app};

async fn call_tool(client: &ApiClient, name: &str, arguments: Value) -> Value {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: "tools/call".into(),
        params: Some(json!({ "name": name, "arguments": arguments })),
    };
    let resp = dispatch_request_with_profile(client, ToolProfile::Full, req)
        .await
        .unwrap();
    assert!(resp.error.is_none(), "tool {name} failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    assert_ne!(result["isError"], true, "tool {name} failed: {result}");
    let text = result["content"][0]["text"].as_str().unwrap().to_owned();
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn plan_command_reads_every_task_with_priority_and_blockers() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token);

    let project = call_tool(
        &client,
        "daruma_project_create",
        json!({ "title": "Sprint" }),
    )
    .await;
    let pid = project["project_id"].as_str().unwrap().to_owned();
    let priorities = ["p1", "p2", "p2", "p3", "p2", "p2", "p0"];
    let tasks: Vec<Value> = priorities
        .iter()
        .enumerate()
        .map(|(i, p)| json!({ "title": format!("Sprint task {}", i + 1), "priority": p }))
        .collect();
    let materialized = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({ "plan": { "title": "Sprint plan", "project_id": pid }, "tasks": tasks }),
    )
    .await;
    let created = |kind: &str, field: &str| -> Vec<String> {
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
    };
    let plan_id = created("plan_created", "plan").remove(0);
    let ids = created("task_created", "task");
    assert_eq!(ids.len(), 7, "{materialized}");
    eprintln!("TRACE plan={plan_id} tasks={ids:?}");

    call_tool(
        &client,
        "daruma_plan_set_status",
        json!({ "plan_id": plan_id, "status": "active" }),
    )
    .await;
    let done = call_tool(
        &client,
        "daruma_set_status",
        json!({ "id": ids[0], "status": "done" }),
    )
    .await;
    eprintln!("TRACE task1 done: {}", done["success"]);
    call_tool(
        &client,
        "daruma_link",
        json!({ "from": ids[2], "to": ids[5], "kind": "blocks" }),
    )
    .await;

    // Before: the capped summary the /plan command used to read.
    let progress = call_tool(&client, "daruma_plan_get", json!({ "id": plan_id })).await;
    eprintln!("TRACE view=progress {progress}");
    let listed: usize = ["active", "blocked", "next"]
        .iter()
        .map(|k| progress[*k].as_array().map_or(0, Vec::len))
        .sum();
    assert!(listed < 7, "progress view is a capped summary: {progress}");

    // After: the full list /plan now asks for.
    let view = call_tool(
        &client,
        "daruma_plan_get",
        json!({ "id": plan_id, "view": "tasks" }),
    )
    .await;
    eprintln!("TRACE view=tasks {view}");
    assert_eq!(view["plan"]["id"], json!(plan_id));
    assert_eq!(view["progress"]["tasks_total"], 7);
    assert_eq!(view["progress"]["tasks_done"], 1);
    let rows = view["tasks"].as_array().expect("tasks array");
    assert_eq!(rows.len(), 7, "every task, done included: {view}");
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row["task_id"], json!(ids[i]), "plan order: {view}");
        assert_eq!(
            row["priority"],
            json!(priorities[i]),
            "priority of task {}: {view}",
            i + 1
        );
    }
    assert_eq!(rows[0]["status"], "done");
    assert_eq!(rows[0]["blocked_by"], json!([]));
    assert_eq!(
        rows[5]["blocked_by"],
        json!([ids[2]]),
        "blocks relation surfaces: {view}"
    );
    assert_eq!(rows[6]["blocked_by"], json!([]));
}
