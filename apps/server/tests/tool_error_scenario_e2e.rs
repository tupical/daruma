//! Plan 01a06cfc, task 04d91f7d — an agent working a task over the hosted
//! `POST /v1/mcp` endpoint makes a wrong call. The failure must reach it as
//! a tool result (`isError: true` + the server's message, HTTP 200), not as
//! a JSON-RPC `-32603`, so it can read the reason and self-correct. The
//! rejected call leaves no side effect; the corrected call goes through.

use serde_json::{json, Value};

mod common;
use common::{spawn_server, test_app};

struct Agent {
    http: reqwest::Client,
    url: String,
    token: String,
}

impl Agent {
    /// One `tools/call` frame; returns the JSON-RPC body and the request id
    /// the server stamped on it (the trace handle for this hop).
    async fn call(&self, name: &str, arguments: Value) -> (Value, String) {
        let response = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{name}: HTTP status");
        let trace = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        (response.json().await.unwrap(), trace)
    }

    async fn ok(&self, name: &str, arguments: Value) -> Value {
        let (body, trace) = self.call(name, arguments).await;
        assert!(body["error"].is_null(), "{name}: {body}");
        assert_ne!(body["result"]["isError"], true, "{name}: {body}");
        println!("TRACE {name} ok request_id={trace}");
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    async fn tool_error(&self, name: &str, arguments: Value) -> String {
        let (body, trace) = self.call(name, arguments).await;
        assert!(
            body["error"].is_null(),
            "{name}: a tool failure must not be a JSON-RPC error: {body}"
        );
        assert_eq!(body["result"]["isError"], true, "{name}: {body}");
        assert_eq!(body["result"]["content"][0]["type"], "text", "{body}");
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        println!("TRACE {name} isError request_id={trace} text={text}");
        text.to_owned()
    }
}

#[tokio::test]
async fn agent_reads_tool_error_and_self_corrects_over_hosted_mcp() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let agent = Agent {
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
        url: format!("http://{addr}/v1/mcp?profile=full"),
        token: app.admin_token.clone(),
    };

    let project = agent
        .ok("daruma_project_create", json!({ "title": "Tool errors" }))
        .await;
    let pid = project["project_id"].as_str().unwrap().to_owned();
    let materialized = agent
        .ok(
            "daruma_plan_materialize",
            json!({
                "plan": { "title": "Error protocol", "project_id": pid },
                "tasks": [ { "title": "Wire isError" } ]
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
    agent
        .ok(
            "daruma_plan_set_status",
            json!({ "plan_id": plan_id, "status": "active" }),
        )
        .await;
    let drained = agent
        .ok(
            "daruma_plan_drain_next",
            json!({ "plan_id": plan_id, "claim_ttl_secs": 600 }),
        )
        .await;
    assert_eq!(drained["task_id"], task_id.as_str(), "drain: {drained}");
    let before = agent.ok("daruma_get", json!({ "id": task_id })).await;
    assert_eq!(before["status"], "in_progress", "{before}");

    // Mistake 1: a status the tracker does not have. The agent gets the
    // server's reason as text instead of an opaque protocol error.
    let reason = agent
        .tool_error(
            "daruma_set_status",
            json!({ "id": task_id, "status": "finished" }),
        )
        .await;
    assert!(reason.contains("finished"), "{reason}");

    // Mistake 2: a tool that does not exist.
    let unknown = agent
        .tool_error("daruma_finish_task", json!({ "id": task_id }))
        .await;
    assert!(unknown.contains("unknown tool"), "{unknown}");

    // No side effect: the task did not move.
    let after_errors = agent.ok("daruma_get", json!({ "id": task_id })).await;
    assert_eq!(after_errors["status"], "in_progress", "{after_errors}");
    assert_eq!(
        after_errors["updated_event_seq"], before["updated_event_seq"],
        "a rejected call must not write"
    );

    // Self-correction: the valid handoff goes through.
    let handoff = agent
        .ok(
            "daruma_set_status",
            json!({ "id": task_id, "status": "in_review" }),
        )
        .await;
    assert_eq!(handoff["success"], true, "handoff: {handoff}");
    let task = agent.ok("daruma_get", json!({ "id": task_id })).await;
    assert_eq!(task["status"], "in_review", "{task}");
    println!(
        "scenario plan_id={plan_id} task_id={task_id} run_id={} final_status={}",
        drained["run_id"], task["status"]
    );
}
