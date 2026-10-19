//! Plan 01a14418, task ea2028f1 — an agent reads a large task and plan through
//! one MCP session (one `ApiClient`, as on stdio) with `dedup: true` and a
//! small `max_tokens`. The excerpt must not count as "received": the next
//! dedup read of the unchanged object returns content again, never the
//! `unchanged` marker, until the session has been given the full object.

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

fn trace(step: &str, out: &Value) {
    println!(
        "TRACE {step}: unchanged={} truncation={} bytes={}",
        out.get("unchanged").is_some(),
        out.get("truncation").is_some(),
        out.to_string().len()
    );
}

#[tokio::test]
async fn dedup_never_answers_unchanged_for_an_excerpt_only_read() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    // One client for the whole scenario = one stdio MCP session.
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token);

    let project = call_tool(
        &client,
        "daruma_project_create",
        json!({ "title": "Dedup" }),
    )
    .await;
    let pid = project["project_id"].as_str().unwrap().to_owned();
    let description = "Acceptance detail. ".repeat(400);
    let goal = "Plan goal prose. ".repeat(400);
    let materialized = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": { "title": "Dedup plan", "project_id": pid, "goal": goal },
            "tasks": [ { "title": "Large task", "description": description } ]
        }),
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
    println!("TRACE plan={plan_id} task={task_id}");

    // ── Task: excerpt, excerpt again, full, then dedup hit ──────────────────
    let bounded = json!({ "id": task_id, "dedup": true, "max_tokens": 200 });
    let first = call_tool(&client, "daruma_get", bounded.clone()).await;
    trace("task.1 bounded", &first);
    assert!(
        first.get("truncation").is_some(),
        "excerpt expected: {first}"
    );

    let second = call_tool(&client, "daruma_get", bounded.clone()).await;
    trace("task.2 bounded again", &second);
    assert!(
        second.get("unchanged").is_none(),
        "excerpt must not be credited as received: {second}"
    );
    assert!(second.get("truncation").is_some());

    let full = call_tool(
        &client,
        "daruma_get",
        json!({ "id": task_id, "dedup": true }),
    )
    .await;
    trace("task.3 full", &full);
    assert!(full.get("unchanged").is_none());
    assert_eq!(full["description"].as_str().unwrap(), description);

    let hit = call_tool(&client, "daruma_get", bounded).await;
    trace("task.4 after full", &hit);
    assert_eq!(hit["unchanged"], true, "full delivery is remembered: {hit}");
    assert_eq!(hit["ref"], task_id.as_str());

    // ── Plan detail: same contract ──────────────────────────────────────────
    let plan_bounded = json!({ "id": plan_id, "view": "detail", "dedup": true, "max_tokens": 200 });
    let p1 = call_tool(&client, "daruma_plan_get", plan_bounded.clone()).await;
    trace("plan.1 bounded", &p1);
    assert!(
        p1.get("truncation").is_some(),
        "plan excerpt expected: {p1}"
    );
    let p2 = call_tool(&client, "daruma_plan_get", plan_bounded.clone()).await;
    trace("plan.2 bounded again", &p2);
    assert!(
        p2.get("unchanged").is_none(),
        "plan excerpt not received: {p2}"
    );

    let p3 = call_tool(
        &client,
        "daruma_plan_get",
        json!({ "id": plan_id, "view": "detail", "dedup": true }),
    )
    .await;
    trace("plan.3 full", &p3);
    assert_eq!(p3["plan"]["goal"].as_str().unwrap(), goal);
    let p4 = call_tool(&client, "daruma_plan_get", plan_bounded).await;
    trace("plan.4 after full", &p4);
    assert_eq!(p4["unchanged"], true, "plan full delivery remembered: {p4}");
}

fn materialize(materialized: &Value, kind: &str, field: &str) -> String {
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
}

/// Task ea7e8701 — adding a task to a plan changes the plan detail
/// (`progress`, `latest_activity_at`) without bumping `plan.updated_at`. The
/// next `dedup: true` read must deliver the new summary, not `unchanged`.
#[tokio::test]
async fn dedup_plan_detail_is_not_unchanged_after_a_task_is_added() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token);

    let project = call_tool(
        &client,
        "daruma_project_create",
        json!({ "title": "Dedup membership" }),
    )
    .await;
    let pid = project["project_id"].as_str().unwrap().to_owned();
    let main = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": { "title": "Release", "project_id": pid, "goal": "Ship" },
            "tasks": [ { "title": "Write notes" } ]
        }),
    )
    .await;
    let plan_id = materialize(&main, "plan_created", "plan");
    let other = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": { "title": "Backlog", "project_id": pid, "goal": "Later" },
            "tasks": [ { "title": "Tag the release" } ]
        }),
    )
    .await;
    let added_task = materialize(&other, "task_created", "task");
    println!("TRACE plan={plan_id} added_task={added_task}");

    let detail = json!({ "id": plan_id, "view": "detail", "dedup": true });
    let d1 = call_tool(&client, "daruma_plan_get", detail.clone()).await;
    trace("membership.1 full", &d1);
    assert!(d1.get("unchanged").is_none());
    assert_eq!(d1["progress"]["tasks_total"], 1, "{d1}");
    let d2 = call_tool(&client, "daruma_plan_get", detail.clone()).await;
    trace("membership.2 repeat", &d2);
    assert_eq!(d2["unchanged"], true, "unchanged plan dedups: {d2}");

    call_tool(
        &client,
        "daruma_plan_add_task",
        json!({ "plan_id": plan_id, "task_id": added_task }),
    )
    .await;

    let d3 = call_tool(&client, "daruma_plan_get", detail.clone()).await;
    trace("membership.3 after add", &d3);
    println!(
        "TRACE plan.updated_at before={} after={}",
        d1["plan"]["updated_at"], d3["plan"]["updated_at"]
    );
    assert!(
        d3.get("unchanged").is_none(),
        "task added: the summary changed, so no false unchanged: {d3}"
    );
    assert_eq!(d3["progress"]["tasks_total"], 2, "{d3}");

    let d4 = call_tool(&client, "daruma_plan_get", detail).await;
    trace("membership.4 repeat", &d4);
    assert_eq!(d4["unchanged"], true, "new summary is remembered: {d4}");
    assert_eq!(d4["since"], d3["plan"]["updated_at"], "{d4}");
}
