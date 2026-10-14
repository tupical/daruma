//! Corporate scenario over a real HTTP server and the MCP dispatcher: a
//! second plan on the branch of a just-completed plan is not created
//! silently. Without a choice the agent gets `plan_choice_required` with the
//! candidate (what it is, how fresh, its source chain and last tasks); it
//! then either continues that plan with `plan.id` or, for another subject,
//! creates a new one with `plan.separate`.

use daruma_mcp::{dispatch_request_with_profile, ApiClient, JsonRpcRequest, ToolProfile};
use serde_json::{json, Value};

mod common;
use common::{spawn_server, TestAppBuilder};

const ISSUE: &str = "https://gitlab.romashka.example/portal/web/-/issues/11542";
const BRANCH: &str = "11542_av_statuses";

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

fn created(resp: &Value, kind: &str) -> Vec<String> {
    resp["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["payload"]["type"] == kind)
        .filter_map(|e| {
            e["payload"]["task"]["id"]
                .as_str()
                .or(e["payload"]["plan"]["id"].as_str())
                .map(str::to_owned)
        })
        .collect()
}

fn meeting(project_id: &str, label: &str, title: &str, extra: Value) -> Value {
    let mut plan = json!({
        "title": title,
        "project_id": project_id,
        "source": { "label": label, "upstream": [{ "ref": ISSUE }] },
        "git_context": { "repo": "portal/web", "branch": BRANCH }
    });
    for (k, v) in extra.as_object().unwrap() {
        plan[k] = v.clone();
    }
    plan
}

#[tokio::test]
async fn second_plan_on_a_branch_needs_an_explicit_choice() {
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
    let plan_count = || async {
        ok(
            &client,
            "daruma_plan_list",
            json!({ "project_id": project_id, "status": "all" }),
        )
        .await["items"]
            .as_array()
            .unwrap()
            .len()
    };

    // 1. After the director's meeting the team plans the statuses work on
    // the issue's branch and finishes it.
    let first = ok(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": meeting(&project_id, "Совещание с директором", "Статусы заявок", json!({})),
            "tasks": [{ "title": "Добавить статус «На проверке»" }],
            "verbose": true
        }),
    )
    .await;
    let statuses = created(&first, "plan_created").remove(0);
    for id in created(&first, "task_created") {
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
        json!({ "plan_id": statuses, "status": "completed" }),
    )
    .await;
    println!("TRACE statuses plan={statuses} completed");

    // 2. Minutes later a follow-up from the dialogue backlog, same branch,
    // no choice made. Expected: refused, nothing written, the completed
    // statuses plan is listed with its source chain and last task.
    let err = tool(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": meeting(&project_id, "Бэклог диалога", "Статусы: уведомления", json!({})),
            "tasks": [{ "title": "Уведомлять клиента о смене статуса" }]
        }),
    )
    .await
    .expect_err("a second plan on the branch needs a choice");
    println!("TRACE refusal: {err}");
    assert!(err.contains("plan_choice_required"), "{err}");
    assert!(err.contains(&statuses), "candidate listed: {err}");
    assert!(err.contains("Совещание с директором"), "label chain: {err}");
    assert!(err.contains("Добавить статус"), "recent task: {err}");
    assert!(err.contains("plan.separate"), "hint: {err}");
    assert_eq!(plan_count().await, 1, "a refusal writes nothing");

    // 3. Same subject: the agent continues the statuses plan by id.
    let appended = ok(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": { "id": statuses, "source": { "label": "Бэклог диалога" } },
            "tasks": [{ "title": "Уведомлять клиента о смене статуса" }],
            "verbose": true
        }),
    )
    .await;
    assert!(created(&appended, "plan_created").is_empty(), "{appended}");
    let progress = ok(&client, "daruma_plan_get", json!({ "id": statuses })).await;
    println!("TRACE after append: {progress}");
    assert_eq!(progress["plan"]["status"], "active", "{progress}");
    assert_eq!(progress["progress"]["tasks_total"], 2, "{progress}");
    assert_eq!(plan_count().await, 1);

    // 4. Another subject on the same branch: the agent says so explicitly.
    let separate = ok(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": meeting(&project_id, "Бэклог диалога", "Резолвер стадий", json!({ "separate": true })),
            "tasks": [{ "title": "Описать стадии резолвера" }],
            "verbose": true
        }),
    )
    .await;
    let resolver = created(&separate, "plan_created");
    println!("TRACE separate plan={resolver:?}");
    assert_eq!(resolver.len(), 1, "{separate}");
    assert_eq!(plan_count().await, 2);
}
