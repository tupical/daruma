//! Corporate scenario (task 01a144c6) over a real HTTP server and the MCP
//! dispatcher: a team with twelve active plans resumes work on an older one;
//! `/next` must pick that plan. `/v1/plans` lists oldest-created first and
//! the tool pages by ten, so the old "most recent of the first page" pick
//! missed it; `sort=updated limit=1` returns it.

use daruma_mcp::{dispatch_request_with_profile, ApiClient, JsonRpcRequest, ToolProfile};
use serde_json::{json, Value};

mod common;
use common::{spawn_server, TestAppBuilder};

async fn rpc(client: &ApiClient, method: &str, params: Value) -> Value {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: method.into(),
        params: Some(params),
    };
    let resp = dispatch_request_with_profile(client, ToolProfile::Full, req)
        .await
        .unwrap();
    assert!(resp.error.is_none(), "{method}: {:?}", resp.error);
    resp.result.unwrap()
}

async fn ok(client: &ApiClient, name: &str, arguments: Value) -> Value {
    let result = rpc(
        client,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
    .await;
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert_ne!(result["isError"], true, "{name} failed: {text}");
    serde_json::from_str(text).unwrap_or_else(|_| json!(text))
}

fn ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn next_picks_the_latest_updated_plan_past_the_first_page() {
    let app = TestAppBuilder::default().build().await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token.clone());
    let project = ok(
        &client,
        "daruma_project_create",
        json!({ "title": "Ромашка: клиентский портал" }),
    )
    .await;
    let project_id = project["project_id"].as_str().unwrap().to_owned();

    // 1. Twelve sprint plans from twelve planning meetings, one task each.
    let mut plans = Vec::new();
    for n in 1..=12 {
        let resp = ok(
            &client,
            "daruma_plan_materialize",
            json!({
                "plan": {
                    "title": format!("Спринт {n}: портал"),
                    "project_id": project_id,
                    "source": { "label": format!("Планёрка спринта {n}") }
                },
                "tasks": [{ "title": format!("Задача спринта {n}") }],
                "verbose": true
            }),
        )
        .await;
        let plan_id = resp["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["payload"]["type"] == "plan_created")
            .and_then(|e| e["payload"]["plan"]["id"].as_str())
            .unwrap_or_else(|| panic!("plan_created in {resp}"))
            .to_owned();
        plans.push(plan_id);
    }
    let resumed = plans[10].clone();
    println!("TRACE project={project_id} plans={plans:?}");

    // 2. The director moves sprint 11 back into focus: its goal is updated
    // after sprint 12 was created, so sprint 11 is the latest-updated plan.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    ok(
        &client,
        "daruma_plan_update",
        json!({ "id": resumed, "patch": { "goal": "Вернуть статусы заявок в релиз" } }),
    )
    .await;
    println!("TRACE resumed plan={resumed} (sprint 11) updated");

    // 3. Expected before the fix: the default first page is sprints 1..10
    // and does not contain sprint 11 — "most recent of that page" is wrong.
    let default_page = ok(
        &client,
        "daruma_plan_list",
        json!({ "project_id": project_id, "status": "active" }),
    )
    .await;
    let default_ids = ids(&default_page);
    println!(
        "TRACE default page={} has_more={}",
        default_ids.len(),
        default_page["has_more"]
    );
    assert_eq!(default_ids, plans[..10]);
    assert!(!default_ids.contains(&resumed));

    // 4. The /next step 2 as the commands now word it.
    let latest = ok(
        &client,
        "daruma_plan_list",
        json!({ "project_id": project_id, "status": "active", "sort": "updated", "limit": 1 }),
    )
    .await;
    println!(
        "TRACE sort=updated limit=1 -> {:?} updated_at={}",
        ids(&latest),
        latest["items"][0]["updated_at"]
    );
    assert_eq!(ids(&latest), [resumed.clone()]);
    assert_eq!(latest["has_more"], true);

    // 5. /next step 3: drain the chosen plan; its task gets claimed.
    let drained = ok(
        &client,
        "daruma_plan_drain_next",
        json!({ "plan_id": resumed, "claim_ttl_secs": 600 }),
    )
    .await;
    println!(
        "TRACE drain plan={} task={} run={}",
        drained["plan_id"], drained["task_id"], drained["run_id"]
    );
    assert_eq!(drained["plan_id"], json!(resumed));
    let task = ok(&client, "daruma_get", json!({ "id": drained["task_id"] })).await;
    assert_eq!(task["title"], "Задача спринта 11");
    assert_eq!(task["status"], "in_progress");

    // 6. The server-side /daruma-next prompt (used by stock Cursor installs)
    // carries the same instruction as the local command files.
    let prompt = rpc(&client, "prompts/get", json!({ "name": "daruma-next" })).await;
    let text = prompt.to_string();
    assert!(
        text.contains(r#"`sort = \"updated\"`, `limit = 1`"#),
        "{text}"
    );
    println!("TRACE prompts/get daruma-next asks for sort=updated");
}
