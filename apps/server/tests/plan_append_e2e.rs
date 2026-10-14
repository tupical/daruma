//! Corporate scenario over a real HTTP server and the MCP dispatcher: one
//! GitLab issue keeps ONE plan. Follow-up work materialised later with
//! `plan.id` lands in that plan (same project, next positions), a completed
//! plan is reopened by the follow-up, a different source is noted on the new
//! task, and an abandoned plan refuses further tasks.

use daruma_mcp::{dispatch_request_with_profile, ApiClient, JsonRpcRequest, ToolProfile};
use serde_json::{json, Value};

mod common;
use common::{spawn_server, TestAppBuilder};

const ISSUE_7: &str = "https://gitlab.romashka.example/portal/web/-/issues/7";
const ISSUE_8: &str = "https://gitlab.romashka.example/portal/web/-/issues/8";

async fn tool(client: &ApiClient, name: &str, arguments: Value) -> Result<Value, String> {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: "tools/call".into(),
        params: Some(json!({ "name": name, "arguments": arguments })),
    };
    let resp = dispatch_request_with_profile(client, ToolProfile::Full, req)
        .await
        .unwrap();
    if let Some(error) = resp.error {
        return Err(format!("{error:?}"));
    }
    let result = resp.result.unwrap();
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    if result["isError"] == true {
        return Err(text.to_owned());
    }
    Ok(serde_json::from_str(text).unwrap_or_else(|_| json!(text)))
}

async fn ok(client: &ApiClient, name: &str, arguments: Value) -> Value {
    tool(client, name, arguments)
        .await
        .unwrap_or_else(|e| panic!("{name} failed: {e}"))
}

/// Ids of the tasks a materialize response created (from the event list).
fn created_tasks(resp: &Value) -> Vec<String> {
    resp["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["payload"]["type"] == "task_created")
        .filter_map(|e| e["payload"]["task"]["id"].as_str().map(str::to_owned))
        .collect()
}

fn event_types(resp: &Value) -> Vec<String> {
    resp["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["payload"]["type"].as_str().map(str::to_owned))
        .collect()
}

async fn progress(client: &ApiClient, plan_id: &str) -> Value {
    ok(client, "daruma_plan_get", json!({ "id": plan_id })).await
}

#[tokio::test]
async fn issue_follow_up_tasks_stay_in_one_plan() {
    let app = TestAppBuilder::default()
        .plan_only_intake(true)
        .build()
        .await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token.clone());

    let project = ok(
        &client,
        "daruma_project_create",
        json!({ "title": "Ромашка: клиентский портал" }),
    )
    .await;
    let project_id = project["project_id"].as_str().unwrap().to_owned();

    // 1. The team plans issue #7 on its branch.
    let first = ok(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": {
                "title": "Issue #7: клиенты не могут войти",
                "project_id": project_id,
                "source": { "ref": ISSUE_7 },
                "git_context": { "branch": "7_fix_login" }
            },
            "tasks": [{ "title": "Воспроизвести ошибку входа" }, { "title": "Починить проверку токена" }],
            "verbose": true
        }),
    )
    .await;
    let plan_id = first["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["payload"]["type"] == "plan_created")
        .and_then(|e| e["payload"]["plan"]["id"].as_str())
        .expect("plan_created")
        .to_owned();
    println!("TRACE plan_id={plan_id} events={:?}", event_types(&first));

    // 2. A follow-up on the same issue/branch: `plan.id`, no title.
    // Expected: no new plan, the task is the plan's 3rd, inherits the branch,
    // and gets no source comment (same source).
    let follow_up = ok(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": { "id": plan_id, "source": { "ref": ISSUE_7 } },
            "tasks": [{ "title": "Покрыть вход регрессионным тестом" }],
            "verbose": true
        }),
    )
    .await;
    let types = event_types(&follow_up);
    println!("TRACE follow-up events={types:?}");
    assert!(!types.contains(&"plan_created".to_string()), "{types:?}");
    assert!(!types.contains(&"comment_added".to_string()), "{types:?}");
    let [regression] = &created_tasks(&follow_up)[..] else {
        panic!("one task expected: {follow_up}");
    };
    let plans = ok(
        &client,
        "daruma_plan_list",
        json!({ "project_id": project_id, "status": "all" }),
    )
    .await;
    assert_eq!(plans["items"].as_array().unwrap().len(), 1, "{plans}");
    let after = progress(&client, &plan_id).await;
    println!("TRACE progress after follow-up: {after}");
    assert_eq!(after["progress"]["tasks_total"], 3, "{after}");
    let task = ok(&client, "daruma_get", json!({ "id": regression })).await;
    assert_eq!(task["project_id"], project_id, "{task}");
    assert_eq!(task["git_context"]["branch"], "7_fix_login", "{task}");

    // 3. All done, plan completed — then QA reopens it from issue #8.
    for id in created_tasks(&first).iter().chain([regression]) {
        ok(
            &client,
            "daruma_set_status",
            json!({ "id": id, "status": "done" }),
        )
        .await;
    }
    ok(
        &client,
        "daruma_plan_set_status",
        json!({ "plan_id": plan_id, "status": "completed" }),
    )
    .await;
    let reopen = ok(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": {
                "id": plan_id,
                "source": { "ref": ISSUE_8, "label": "QA: вход по SSO" },
                "source_brief": "QA нашла сбой SSO после фикса #7"
            },
            "tasks": [{ "title": "Починить вход через SSO" }],
            "verbose": true
        }),
    )
    .await;
    let types = event_types(&reopen);
    println!("TRACE reopen events={types:?}");
    assert_eq!(types[0], "plan_status_changed", "{types:?}");
    let [sso] = &created_tasks(&reopen)[..] else {
        panic!("one task expected: {reopen}");
    };
    let after = progress(&client, &plan_id).await;
    println!("TRACE progress after reopen: {after}");
    assert_eq!(after["plan"]["status"], "active", "{after}");
    assert_eq!(after["progress"]["tasks_total"], 4, "{after}");
    let sso_task = ok(
        &client,
        "daruma_get",
        json!({ "id": sso, "comments": true }),
    )
    .await;
    let note = sso_task["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|c| c["body"].as_str())
        .unwrap_or_else(|| panic!("source note expected: {sso_task}"));
    println!("TRACE source note on {sso}: {note}");
    assert!(note.contains(ISSUE_8), "{note}");
    assert!(note.contains("QA: вход по SSO"), "{note}");

    // 4. The plan is abandoned: a late follow-up is refused, nothing lands.
    ok(
        &client,
        "daruma_plan_set_status",
        json!({ "plan_id": plan_id, "status": "abandoned" }),
    )
    .await;
    let err = tool(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": { "id": plan_id },
            "tasks": [{ "title": "Слишком поздно" }]
        }),
    )
    .await
    .expect_err("abandoned plan must refuse tasks");
    println!("TRACE abandoned refusal: {err}");
    assert!(err.contains("abandoned"), "{err}");
    assert_eq!(
        progress(&client, &plan_id).await["progress"]["tasks_total"],
        4
    );
}
