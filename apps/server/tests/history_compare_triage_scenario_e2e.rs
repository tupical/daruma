//! Plan 01a14418, task ea40b857 — corporate-game scenario against a real
//! inline server with production wiring (rule engine gate + plan-only intake).
//!
//! An intake desk queues a materialized plan task for triage, the triage lead
//! accepts it over `PATCH /v1/tasks/{id}/triage`, the lead raises its priority
//! over MCP, and an auditor compares versions 1 and 4 over
//! `daruma_history_compare`. Expected: both triage decisions are versions of
//! their own, the 1 → 4 compare reports the triage and the priority change
//! together (not only the last `3 → 4` step), and rolling back to version 2
//! restores `triage_state = pending` through the versioned path.

use daruma_mcp::{dispatch_request_with_profile, ApiClient, JsonRpcRequest, ToolProfile};
use serde_json::{json, Value};

mod common;
use common::{spawn_server, TestAppBuilder};

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

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn triage_desk_decisions_are_versioned_and_compare_spans_the_selected_pair() {
    let app = TestAppBuilder::default()
        .rule_engine_gate()
        .plan_only_intake(true)
        .build()
        .await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token.clone());

    let project = call_tool(
        &client,
        "daruma_project_create",
        json!({ "title": "Corporate game: triage desk" }),
    )
    .await;
    let pid = project["project_id"].as_str().unwrap().to_owned();
    let triage_on = client
        .patch_json(
            &format!("/v1/projects/{pid}/triage"),
            json!({ "triage_enabled": true }),
        )
        .await
        .unwrap();
    assert_eq!(triage_on["triage_enabled"], true, "{triage_on}");

    let materialized = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": { "title": "Q4 onboarding", "project_id": pid },
            "tasks": [ { "title": "Prepare the onboarding checklist" } ]
        }),
    )
    .await;
    let task_id = materialized["data"]
        .as_array()
        .and_then(|a| {
            a.iter().find_map(|e| {
                let p = e.get("payload")?;
                (p.get("type")?.as_str()? == "task_created")
                    .then(|| p.get("task")?.get("id")?.as_str().map(str::to_owned))?
            })
        })
        .unwrap_or_else(|| panic!("task_created in {materialized}"));
    println!("TRACE step=materialize project={pid} task={task_id}");

    // Intake desk queues the task for triage; the lead accepts it.
    let pending = client
        .patch_json(
            &format!("/v1/tasks/{task_id}/triage"),
            json!({ "triage_state": "pending" }),
        )
        .await
        .unwrap();
    assert_eq!(pending["triage_state"], "pending", "{pending}");
    let queue = client
        .get_json(&format!("/v1/projects/{pid}/triage"))
        .await
        .unwrap();
    assert_eq!(queue.as_array().map(Vec::len), Some(1), "queue: {queue}");
    let accepted = client
        .patch_json(
            &format!("/v1/tasks/{task_id}/triage"),
            json!({ "triage_state": "accepted" }),
        )
        .await
        .unwrap();
    assert_eq!(accepted["triage_state"], "accepted", "{accepted}");
    println!(
        "TRACE step=triage pending->accepted updated_event_seq={}",
        accepted["updated_event_seq"]
    );

    let priority = call_tool(
        &client,
        "daruma_set_priority",
        json!({ "id": task_id, "priority": "p0" }),
    )
    .await;
    assert_eq!(priority["success"], true, "{priority}");

    let history = call_tool(
        &client,
        "daruma_history_list",
        json!({ "entity_type": "task", "entity_id": task_id }),
    )
    .await;
    let versions = history.as_array().expect("history array");
    for v in versions.iter().rev() {
        println!(
            "TRACE version={} event={} changed={:?} source_event_seq={}",
            v["version_number"],
            v["event_type"],
            strings(&v["changed_fields"]),
            v["source_event_seq"]
        );
    }
    assert_eq!(versions.len(), 4, "create + 2 triage + priority: {history}");
    let triage_versions: Vec<i64> = versions
        .iter()
        .filter(|v| strings(&v["changed_fields"]).contains(&"triage_state"))
        .filter_map(|v| v["version_number"].as_i64())
        .collect();
    assert_eq!(triage_versions, vec![3, 2], "triage versions: {history}");

    let compared = call_tool(
        &client,
        "daruma_history_compare",
        json!({ "entity_type": "task", "entity_id": task_id, "from": 1, "to": 4 }),
    )
    .await;
    let diff = &compared["diff"];
    let changed = strings(&diff["changed_fields"]);
    println!(
        "TRACE step=compare from=1 to=4 changed={changed:?} to_own_step={:?}",
        strings(&compared["to"]["changed_fields"])
    );
    assert!(changed.contains(&"triage_state"), "compare 1→4: {diff}");
    assert!(changed.contains(&"priority"), "compare 1→4: {diff}");
    assert_eq!(
        diff["diff"]["fields"]["triage_state"]["before"],
        Value::Null
    );
    assert_eq!(diff["diff"]["fields"]["triage_state"]["after"], "accepted");
    assert_eq!(diff["diff"]["fields"]["priority"]["after"], "p0");
    assert_eq!(diff["diff"]["metadata"]["source"], "version_compare");
    assert!(
        !strings(&compared["to"]["changed_fields"]).contains(&"triage_state"),
        "version 4 alone is the priority step: {compared}"
    );

    // Roll back to version 2 (pending): restores triage through UpdateTask.
    let v2 = versions
        .iter()
        .find(|v| v["version_number"] == 2)
        .and_then(|v| v["id"].as_str())
        .unwrap()
        .to_owned();
    let rollback = call_tool(
        &client,
        "daruma_history_rollback",
        json!({ "version_id": v2 }),
    )
    .await;
    assert_eq!(rollback["success"], true, "{rollback}");
    let task = call_tool(&client, "daruma_get", json!({ "id": task_id })).await;
    println!(
        "TRACE step=rollback to=v2 triage_state={} priority={} event_seq={}",
        task["triage_state"], task["priority"], rollback["event_seq"]
    );
    assert_eq!(task["triage_state"], "pending", "{task}");
    assert_eq!(task["priority"], "p2", "{task}");
}
