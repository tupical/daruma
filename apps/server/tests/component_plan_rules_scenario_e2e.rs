//! ADR-0013 §3, task 01a1bff3-4b7a-7320-87df-0e67e24686de — an agent reads the
//! planning rules from `initialize` and follows them with the default-profile
//! tools against a real inline server: one plan for a goal across repos, a
//! repo per task, order between components through `blocks`.

use daruma_mcp::{dispatch_request_with_profile, ApiClient, JsonRpcRequest, ToolProfile};
use serde_json::{json, Value};

mod common;
use common::{spawn_server, test_app};

async fn rpc(client: &ApiClient, profile: ToolProfile, method: &str, params: Value) -> Value {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: method.into(),
        params: Some(params),
    };
    let resp = dispatch_request_with_profile(client, profile, req)
        .await
        .unwrap();
    assert!(resp.error.is_none(), "{method} failed: {:?}", resp.error);
    resp.result.unwrap()
}

async fn call_in(client: &ApiClient, profile: ToolProfile, name: &str, arguments: Value) -> Value {
    let result = rpc(
        client,
        profile,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
    .await;
    assert_ne!(result["isError"], true, "tool {name} failed: {result}");
    let text = result["content"][0]["text"].as_str().unwrap().to_owned();
    serde_json::from_str(&text).unwrap()
}

/// The rule's own steps: what any default-profile agent can call.
async fn call_tool(client: &ApiClient, name: &str, arguments: Value) -> Value {
    call_in(client, ToolProfile::Default, name, arguments).await
}

/// Fixture setup, not part of the rules.
async fn fixture_tool(client: &ApiClient, name: &str, arguments: Value) -> Value {
    call_in(client, ToolProfile::Full, name, arguments).await
}

/// The rule's step on a task the agent has just started; the claim stays.
async fn set_repo(client: &ApiClient, task: &str, repo: &str) {
    call_tool(
        client,
        "daruma_update",
        json!({ "id": task, "git_context": { "repo": repo } }),
    )
    .await;
    let read = call_tool(client, "daruma_get", json!({ "id": task })).await;
    eprintln!(
        "TRACE started task={task} status={} git_context={} claim={}",
        read["status"], read["git_context"], read["current_claim"]["is_mine"]
    );
    assert_eq!(read["status"], "in_progress", "{read}");
    assert_eq!(read["git_context"]["repo"], repo, "{read}");
    assert_eq!(read["current_claim"]["is_mine"], true, "{read}");
}

#[tokio::test]
async fn one_plan_across_repos_follows_the_initialize_rules() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token);

    let init = rpc(&client, ToolProfile::Default, "initialize", json!({})).await;
    let rules = init["instructions"].as_str().unwrap();
    eprintln!("TRACE initialize chars={} {rules}", rules.chars().count());
    assert!(rules.contains("Goal across repos = one plan"));

    let project = fixture_tool(
        &client,
        "daruma_project_create",
        json!({ "title": "Cabinet with a bot" }),
    )
    .await;
    let pid = project["project_id"].as_str().unwrap().to_owned();

    // The bot goes first in the plan, so only `blocks` can put the API ahead.
    let materialized = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": {
                "title": "Decision cards in cabinet and bot",
                "project_id": pid,
                "source": { "label": "owner chat" },
                "git_context": { "repo": "tupical/mcpbox.ru", "branch": "work/cards" }
            },
            "tasks": [ { "title": "Bot sends the card" }, { "title": "API serves the card" } ]
        }),
    )
    .await;
    eprintln!("TRACE materialize {materialized}");
    let events = materialized["data"].as_array().expect("events");
    let plan_id = events
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            (p.get("type")?.as_str()? == "plan_created").then(|| p["plan"]["id"].as_str())?
        })
        .expect("plan id")
        .to_owned();
    let task_ids: Vec<String> = events
        .iter()
        .filter_map(|e| {
            let p = e.get("payload")?;
            (p.get("type")?.as_str()? == "task_created")
                .then(|| p["task"]["id"].as_str().map(str::to_owned))?
        })
        .collect();
    assert_eq!(task_ids.len(), 2, "{materialized}");
    let (bot, api) = (task_ids[0].clone(), task_ids[1].clone());

    // Not set while planning: a git_context write starts a todo task.
    for task in [&bot, &api] {
        let read = call_tool(&client, "daruma_get", json!({ "id": task })).await;
        eprintln!("TRACE planned task={task} status={}", read["status"]);
        assert_eq!(read["status"], "todo", "{read}");
    }

    let link = call_tool(
        &client,
        "daruma_link",
        json!({ "from": api, "to": bot, "kind": "blocks" }),
    )
    .await;
    eprintln!("TRACE blocks api={api} bot={bot} {link}");

    let plan = call_tool(&client, "daruma_plan_get", json!({ "id": plan_id })).await;
    eprintln!("TRACE plan {plan}");
    assert_eq!(plan["progress"]["tasks_total"], 2, "one plan: {plan}");

    let activated = fixture_tool(
        &client,
        "daruma_plan_set_status",
        json!({ "id": plan_id, "status": "active" }),
    )
    .await;
    assert_eq!(activated["success"], true, "activate: {activated}");

    let first = call_tool(
        &client,
        "daruma_plan_drain_next",
        json!({ "id": plan_id, "claim_ttl_secs": 600 }),
    )
    .await;
    eprintln!("TRACE drain first {first}");
    assert_eq!(first["task_id"], api.as_str(), "API first: {first}");
    set_repo(&client, &api, "tupical/mcpbox.ru").await;

    let done = call_tool(
        &client,
        "daruma_set_status",
        json!({ "id": api, "status": "done" }),
    )
    .await;
    eprintln!("TRACE api done {done}");

    let second = call_tool(
        &client,
        "daruma_plan_drain_next",
        json!({ "id": plan_id, "claim_ttl_secs": 600 }),
    )
    .await;
    eprintln!("TRACE drain second {second}");
    assert_eq!(second["task_id"], bot.as_str(), "then the bot: {second}");
    set_repo(&client, &bot, "tupical/mcpbox-bot").await;

    // One plan, each started task names where its work lives.
    let api_read = call_tool(&client, "daruma_get", json!({ "id": api })).await;
    assert_eq!(api_read["git_context"]["repo"], "tupical/mcpbox.ru");
    assert_eq!(api_read["status"], "done");
}
