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
                "name": "taskagent_healthz",
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
