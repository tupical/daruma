//! HTTP MCP endpoint smoke tests.

use daruma_auth::{Capabilities, Capability, ProjectFilter};
use daruma_mcp::{tools::call_tool, ApiClient};
use serde_json::json;

mod common;
use common::{mint_pat, spawn_server, test_app};

async fn workspace_agent_id(
    client: &reqwest::Client,
    addr: &std::net::SocketAddr,
    token: &str,
    request_id: u64,
) -> String {
    let body: serde_json::Value = client
        .post(format!("http://{addr}/v1/mcp"))
        .bearer_auth(token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {"name": "daruma_workspace_info", "arguments": {}}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str::<serde_json::Value>(text).unwrap()["mcp_agent_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn http_mcp_dispatches_tool_calls() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/mcp"))
        .bearer_auth(&app.admin_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "daruma_healthz",
                "arguments": {}
            }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let healthz: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(healthz["status"], "ok");
}

#[tokio::test]
async fn hosted_mcp_identity_is_stable_and_bound_to_the_authenticated_principal() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let client = reqwest::Client::new();
    let caps: Capabilities = [Capability::TaskRead].into();
    let (other_token, other_agent_id) = mint_pat(&app.auth_store(), caps, ProjectFilter::All).await;

    let first = workspace_agent_id(&client, &addr, &app.admin_token, 1).await;
    let second = workspace_agent_id(&client, &addr, &app.admin_token, 2).await;
    let other = workspace_agent_id(&client, &addr, &other_token, 3).await;

    assert_eq!(first, app.admin_agent_id.as_uuid().to_string());
    assert_eq!(
        second, first,
        "same bearer principal must keep one MCP identity"
    );
    assert_eq!(other, other_agent_id.as_uuid().to_string());
    assert_ne!(other, first);
}

#[tokio::test]
async fn stdio_dedup_refreshes_on_each_claim_generation_and_release() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let response: serde_json::Value = reqwest::Client::new()
        .post(format!("http://{addr}/v1/commands"))
        .bearer_auth(&app.admin_token)
        .json(&json!({"command": {"type": "create_task", "task": {"title": "dedup claim"}}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let task_id = response["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|event| event["payload"]["task"]["id"].as_str())
        .unwrap()
        .to_string();
    let agent_id = app.admin_agent_id.as_uuid().to_string();
    let api = ApiClient::new(format!("http://{addr}"), app.admin_token.clone())
        .with_agent_id(app.admin_agent_id);
    let get = || call_tool(&api, "daruma_get", json!({"id": task_id, "dedup": true}));

    let first = get().await.unwrap();
    assert_eq!(first["current_claim"], serde_json::Value::Null);
    assert_eq!(get().await.unwrap()["unchanged"], true);

    let first_claim = call_tool(
        &api,
        "daruma_claim",
        json!({"agent_id": agent_id, "task_id": task_id, "ttl_secs": 60}),
    )
    .await
    .unwrap();
    let first_generation = first_claim["data"]["claim_id"].clone();
    let after_claim = get().await.unwrap();
    assert_ne!(after_claim["unchanged"], true);
    assert_eq!(after_claim["current_claim"]["claim_id"], first_generation);
    assert_eq!(get().await.unwrap()["unchanged"], true);

    let refreshed = call_tool(
        &api,
        "daruma_claim",
        json!({"agent_id": agent_id, "task_id": task_id, "ttl_secs": 120}),
    )
    .await
    .unwrap();
    let refreshed_generation = refreshed["data"]["claim_id"].clone();
    assert_ne!(refreshed_generation, first_generation);
    let after_refresh = get().await.unwrap();
    assert_ne!(after_refresh["unchanged"], true);
    assert_eq!(
        after_refresh["current_claim"]["claim_id"],
        refreshed_generation
    );

    call_tool(
        &api,
        "daruma_release",
        json!({"agent_id": agent_id, "task_id": task_id}),
    )
    .await
    .unwrap();
    let after_release = get().await.unwrap();
    assert_ne!(after_release["unchanged"], true);
    assert_eq!(after_release["current_claim"], serde_json::Value::Null);
}

#[tokio::test]
async fn http_mcp_profile_query_param_selects_surface() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let client = reqwest::Client::new();

    // Default (no query param): advanced tool is hidden and not callable.
    let body: serde_json::Value = client
        .post(format!("http://{addr}/v1/mcp"))
        .bearer_auth(&app.admin_token)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "daruma_history_latest", "arguments": {} }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("not available"), "got: {body}");

    // ?profile=full: the same call dispatches.
    let body: serde_json::Value = client
        .post(format!("http://{addr}/v1/mcp?profile=full"))
        .bearer_auth(&app.admin_token)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "daruma_history_latest", "arguments": { "limit": 1 } }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        body["error"].is_null(),
        "full profile must dispatch: {body}"
    );

    // Unknown profile → validation error.
    let resp = client
        .post(format!("http://{addr}/v1/mcp?profile=bogus"))
        .bearer_auth(&app.admin_token)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}
