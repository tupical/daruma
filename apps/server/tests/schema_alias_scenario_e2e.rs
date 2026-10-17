//! Plan 01a06cfc, task 04eacc9f — an agent that builds its calls from the
//! hosted `tools/list` schemas, not from memory. Every key task/plan tool
//! now lists both the canonical id name and the alias its resolver accepts,
//! so an agent that picks the alias (`daruma_comment {id}`, `daruma_get
//! {task_id}`, `daruma_plan_drain_next {id}`, …) works a task end to end
//! over `POST /v1/mcp`. Expected: every alias-only call succeeds, the task
//! ends `in_review` with the comment and no claim; a call with neither
//! name is an `isError` result naming the accepted names, with no write.

use serde_json::{json, Value};

mod common;
use common::{spawn_server, test_app};

struct Agent {
    http: reqwest::Client,
    url: String,
    token: String,
}

impl Agent {
    /// One JSON-RPC frame; returns the body and the server's request id
    /// (the trace handle for this hop).
    async fn rpc(&self, method: &str, params: Value) -> (Value, String) {
        let response = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{method}: HTTP status");
        let trace = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        (response.json().await.unwrap(), trace)
    }

    async fn call(&self, name: &str, arguments: Value) -> (Value, String) {
        self.rpc(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }

    async fn ok(&self, name: &str, arguments: Value) -> Value {
        let (body, trace) = self.call(name, arguments.clone()).await;
        assert!(body["error"].is_null(), "{name}: {body}");
        assert_ne!(
            body["result"]["isError"], true,
            "{name} {arguments}: {body}"
        );
        println!("TRACE {name} {arguments} ok request_id={trace}");
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    async fn tool_error(&self, name: &str, arguments: Value) -> String {
        let (body, trace) = self.call(name, arguments).await;
        assert!(body["error"].is_null(), "{name}: {body}");
        assert_eq!(body["result"]["isError"], true, "{name}: {body}");
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        println!("TRACE {name} isError request_id={trace} text={text}");
        text.to_owned()
    }
}

#[tokio::test]
async fn agent_works_a_task_with_the_aliases_tools_list_advertises() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let admin = Agent {
        http: http.clone(),
        url: format!("http://{addr}/v1/mcp?profile=full"),
        token: app.admin_token.clone(),
    };
    // The working agent sees the default profile, as hosted clients do.
    let agent = Agent {
        http,
        url: format!("http://{addr}/v1/mcp"),
        token: app.admin_token.clone(),
    };

    let project = admin
        .ok(
            "daruma_project_create",
            json!({ "title": "Schema aliases" }),
        )
        .await;
    let pid = project["project_id"].as_str().unwrap().to_owned();
    let materialized = admin
        .ok(
            "daruma_plan_materialize",
            json!({
                "plan": { "title": "Alias schemas", "project_id": pid },
                "tasks": [ { "title": "Advertise id aliases" } ]
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
    admin
        .ok(
            "daruma_plan_set_status",
            json!({ "plan_id": plan_id, "status": "active" }),
        )
        .await;

    // The agent reads the catalogue and takes, for each tool, the id name
    // that is NOT the tool's canonical one — the alias.
    let (listed, trace) = agent.rpc("tools/list", json!({})).await;
    println!("TRACE tools/list request_id={trace}");
    let tools = listed["result"]["tools"].as_array().unwrap();
    let alias = |tool: &str, canonical: &str| -> String {
        let schema = &tools
            .iter()
            .find(|t| t["name"] == tool)
            .unwrap_or_else(|| panic!("{tool} not in default tools/list"))["inputSchema"];
        let required = schema["required"].as_array().cloned().unwrap_or_default();
        let names: Vec<&String> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|k| ["id", "task_id", "plan_id"].contains(&k.as_str()))
            .collect();
        assert!(names.iter().any(|n| *n == canonical), "{tool}: {names:?}");
        assert!(
            !required
                .iter()
                .any(|r| names.iter().any(|n| *r == json!(n))),
            "{tool}: an id name is still required: {required:?}"
        );
        names
            .into_iter()
            .find(|n| *n != canonical)
            .unwrap_or_else(|| panic!("{tool}: no alias advertised"))
            .clone()
    };

    // Task 01a14ae6: daruma_plan_next_task (full profile) lists `plan_id`
    // beside `id`, so a schema-validating client peeks with the alias and
    // no claim is taken.
    let (full, trace) = admin.rpc("tools/list", json!({})).await;
    println!("TRACE tools/list profile=full request_id={trace}");
    let schema = &full["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "daruma_plan_next_task")
        .expect("daruma_plan_next_task in full tools/list")["inputSchema"];
    assert_eq!(
        schema["properties"]["plan_id"]["type"], "string",
        "{schema}"
    );
    assert!(schema["required"].is_null(), "{schema}");
    let peeked = admin
        .ok("daruma_plan_next_task", json!({ "plan_id": plan_id }))
        .await;
    assert_eq!(peeked["task_id"], task_id.as_str(), "peek: {peeked}");
    assert!(
        peeked["claim_expires_at"].is_null(),
        "peek claims: {peeked}"
    );

    let drained = agent
        .ok(
            "daruma_plan_drain_next",
            json!({ alias("daruma_plan_drain_next", "plan_id"): plan_id, "claim_ttl_secs": 600 }),
        )
        .await;
    assert_eq!(drained["task_id"], task_id.as_str(), "drain: {drained}");
    let agent_id = drained["claim"]["agent_id"].as_str().unwrap().to_owned();

    agent
        .ok(
            "daruma_comment",
            json!({ alias("daruma_comment", "task_id"): task_id, "body": "picked via alias", "kind": "intent" }),
        )
        .await;
    let task = agent
        .ok(
            "daruma_get",
            json!({ alias("daruma_get", "id"): task_id, "comments": true }),
        )
        .await;
    assert_eq!(task["status"], "in_progress", "{task}");
    let comments = task["comments"].as_array().unwrap().len();
    assert!(
        task["comments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["body"] == "picked via alias"),
        "{task}"
    );
    agent
        .ok(
            "daruma_relations",
            json!({ alias("daruma_relations", "task_id"): task_id }),
        )
        .await;
    agent
        .ok(
            "daruma_plan_progress",
            json!({ alias("daruma_plan_progress", "plan_id"): plan_id }),
        )
        .await;

    // Neither name: an isError naming both, and nothing is written.
    let missing = agent
        .tool_error("daruma_comment", json!({ "body": "lost" }))
        .await;
    assert!(
        missing.contains("`task_id` (string) is required (alias: `id`)"),
        "{missing}"
    );

    agent
        .ok(
            "daruma_set_status",
            json!({ alias("daruma_set_status", "id"): task_id, "status": "in_review" }),
        )
        .await;
    agent
        .ok(
            "daruma_release",
            json!({ alias("daruma_release", "task_id"): task_id, "agent_id": agent_id }),
        )
        .await;
    let plan = agent
        .ok(
            "daruma_plan_get",
            json!({ alias("daruma_plan_get", "id"): plan_id }),
        )
        .await;

    let task = agent
        .ok("daruma_get", json!({ "id": task_id, "comments": true }))
        .await;
    assert_eq!(task["status"], "in_review", "{task}");
    assert!(task["current_claim"].is_null(), "claim released: {task}");
    assert_eq!(
        task["comments"].as_array().unwrap().len(),
        comments,
        "the rejected comment must not write"
    );
    println!(
        "scenario plan_id={plan_id} task_id={task_id} run_id={} final_status={} plan={}",
        drained["run_id"], task["status"], plan["plan"]["status"]
    );
}
