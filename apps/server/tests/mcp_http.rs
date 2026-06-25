//! HTTP MCP endpoint smoke tests.

use serde_json::json;

mod common;
use common::{spawn_server, test_app};

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
