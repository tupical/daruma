//! Version-history read API and MCP tool coverage.

use axum::http::StatusCode;
use serde_json::{json, Value};
use taskagent_auth::{Capabilities, Capability, TokenKind};
use taskagent_mcp::{dispatch_request, ApiClient, JsonRpcRequest};

mod common;
use common::{json_get, json_post, mint_with_caps, spawn_server, test_app};

fn task_id_from_response(resp: &Value) -> String {
    resp["data"]
        .as_array()
        .expect("data array")
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == "task_created" {
                p.get("task")?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("task_created event")
}

async fn create_and_update_task(app: &axum::Router, token: &str) -> String {
    let (status, create) = json_post(
        app.clone(),
        token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"History v1"}}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create failed: {create}");
    let task_id = task_id_from_response(&create);

    let body = json!({
        "command": {
            "type": "update_task",
            "id": task_id,
            "patch": {
                "title": "History v2"
            }
        }
    });
    let (status, update) = json_post(app.clone(), token, "/v1/commands", &body.to_string()).await;
    assert_eq!(status, StatusCode::OK, "update failed: {update}");

    task_id
}

fn req(method: &str, params: Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: method.into(),
        params: Some(params),
    }
}

async fn call_tool(client: &ApiClient, name: &str, arguments: Value) -> Value {
    let resp = dispatch_request(
        client,
        req(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        ),
    )
    .await
    .unwrap();
    assert!(resp.error.is_none(), "tool {name} failed: {:?}", resp.error);
    let text = resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn history_http_endpoints_expose_versions_and_compare() {
    let h = test_app().await;
    let task_id = create_and_update_task(&h.router, &h.admin_token).await;

    let (status, history) = json_get(
        h.router.clone(),
        &h.admin_token,
        &format!("/v1/history?entity_type=task&entity_id={task_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "history failed: {history}");
    let versions = history.as_array().expect("history array");
    assert_eq!(versions.len(), 2, "create + update versions: {history}");
    assert_eq!(versions[0]["version_number"], 2);
    assert_eq!(
        versions[0]["diff"]["fields"]["title"]["after"],
        "History v2"
    );

    let version_id = versions[0]["id"].as_str().unwrap();
    let (status, got) = json_get(
        h.router.clone(),
        &h.admin_token,
        &format!("/v1/history/{version_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get failed: {got}");
    assert_eq!(got["id"], version_id);

    let (status, compared) = json_get(
        h.router.clone(),
        &h.admin_token,
        &format!("/v1/history/compare?entity_type=task&entity_id={task_id}&from=1&to=2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "compare failed: {compared}");
    assert_eq!(compared["diff"]["from_version"], 1);
    assert_eq!(compared["diff"]["to_version"], 2);

    let rollback_id = versions[1]["id"].as_str().unwrap();
    let (status, rollback) = json_post(
        h.router.clone(),
        &h.admin_token,
        &format!("/v1/history/{rollback_id}/rollback"),
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {rollback}");
    assert_eq!(rollback["success"], true);

    let (status, history_after_rollback) = json_get(
        h.router.clone(),
        &h.admin_token,
        &format!("/v1/history?entity_type=task&entity_id={task_id}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "history failed: {history_after_rollback}"
    );
    let versions_after_rollback = history_after_rollback.as_array().unwrap();
    assert_eq!(versions_after_rollback.len(), 3);
    assert_eq!(versions_after_rollback[0]["reason"], "rollback");
    assert_eq!(
        versions_after_rollback[0]["diff"]["metadata"]["rollback_of_version_id"],
        rollback_id
    );
    assert_eq!(versions_after_rollback[0]["after"]["title"], "History v1");

    let (status, summary) = json_get(
        h.router.clone(),
        &h.admin_token,
        &format!("/v1/history/summary?entity_type=task&entity_id={task_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "summary failed: {summary}");
    assert_eq!(summary["items"].as_array().unwrap().len(), 3);

    let (status, latest) = json_get(h.router, &h.admin_token, "/v1/history/latest?limit=1").await;
    assert_eq!(status, StatusCode::OK, "latest failed: {latest}");
    assert_eq!(latest.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn history_mcp_tools_forward_read_api() {
    let app = test_app().await;
    let task_id = create_and_update_task(&app.router, &app.admin_token).await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token.clone());

    let history = call_tool(
        &client,
        "taskagent_history_list",
        json!({ "entity_type": "task", "entity_id": task_id }),
    )
    .await;
    assert_eq!(history.as_array().unwrap().len(), 2);
    let rollback_id = history.as_array().unwrap()[1]["id"].as_str().unwrap();

    let rollback = call_tool(
        &client,
        "taskagent_history_rollback",
        json!({ "version_id": rollback_id }),
    )
    .await;
    assert_eq!(rollback["success"], true);

    let latest = call_tool(&client, "taskagent_history_latest", json!({ "limit": 5 })).await;
    assert!(!latest.as_array().unwrap().is_empty());

    let summary = call_tool(
        &client,
        "taskagent_history_summary",
        json!({ "entity_type": "task", "entity_id": task_id }),
    )
    .await;
    assert_eq!(summary["items"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn history_rollback_requires_write_capability() {
    let h = test_app().await;
    let task_id = create_and_update_task(&h.router, &h.admin_token).await;
    let (read_only_token, _) = mint_with_caps(
        &h.auth_store(),
        TokenKind::Pat,
        Capabilities::from([Capability::TaskRead]),
    )
    .await;

    let (status, history) = json_get(
        h.router.clone(),
        &read_only_token,
        &format!("/v1/history?entity_type=task&entity_id={task_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "read should be allowed: {history}");
    let rollback_id = history.as_array().unwrap()[1]["id"].as_str().unwrap();

    let (status, rollback) = json_post(
        h.router,
        &read_only_token,
        &format!("/v1/history/{rollback_id}/rollback"),
        "{}",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "rollback must require task write: {rollback}"
    );
}
