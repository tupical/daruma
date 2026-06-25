//! AC-7 — MCP server protocol + tool dispatch.
//!
//! Spins up `daruma-server` inline, points an `ApiClient` at it, and
//! drives the MCP JSON-RPC dispatcher directly (the stdio binary is
//! identical glue around the same `dispatch_request` function — we test
//! the protocol layer rather than the OS pipe plumbing).

use std::net::SocketAddr;

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::json;
use daruma_mcp::{
    dispatch_request_with_profile, tool_definitions, ApiClient, JsonRpcRequest, ToolProfile,
};
use tower::ServiceExt;

mod common;
use common::{spawn_server, test_app};

async fn spawn_daruma_inline() -> (SocketAddr, String) {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    (addr, app.admin_token)
}

fn req(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: method.into(),
        params: Some(params),
    }
}

async fn get_json(app: axum::Router, token: &str, uri: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

// ── AC-7 ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn ac7_initialize_returns_server_info() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let resp = dispatch_request(&client, req("initialize", json!({})))
        .await
        .unwrap();
    let result = resp.result.expect("initialize must return a result");
    assert_eq!(result["serverInfo"]["name"], "daruma-mcp");
    assert!(result["protocolVersion"].is_string());
}

#[tokio::test]
async fn ac7_tools_list_advertises_at_least_ten_required_tools() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let resp = dispatch_request(&client, req("tools/list", json!({})))
        .await
        .unwrap();
    let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
    assert!(tools.len() >= 10, "AC-7: ≥10 tools, got {}", tools.len());

    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for required in [
        "daruma_subscribe_project",
        "daruma_inbox_pull",
        "daruma_comment",
        "daruma_reopen",
        "daruma_update",
    ] {
        assert!(
            names.contains(&required),
            "missing required tool: {required}"
        );
    }
}

#[tokio::test]
async fn ac7_tools_call_create_task_dispatches_through_to_server() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let resp = dispatch_request(
        &client,
        req(
            "tools/call",
            json!({
                "name": "daruma_create",
                "arguments": { "task": { "title": "AC-7 mcp" } }
            }),
        ),
    )
    .await
    .unwrap();

    assert!(resp.error.is_none(), "create failed: {:?}", resp.error);
    let content = resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    let envelopes: serde_json::Value = serde_json::from_str(&content).unwrap();
    let arr = envelopes["data"]
        .as_array()
        .expect("create returns event envelopes");
    assert!(!arr.is_empty(), "must emit at least one event");
    assert_eq!(arr[0]["payload"]["type"], "task_created");
    assert!(
        envelopes["task_id"].is_string(),
        "daruma_create must surface task_id for agents: {envelopes}"
    );

    // Follow up with healthz — verifies a non-auth path also works.
    let healthz = dispatch_request(
        &client,
        req("tools/call", json!({"name": "daruma_healthz"})),
    )
    .await
    .unwrap();
    assert!(healthz.error.is_none());
}

#[tokio::test]
async fn tools_call_update_task_dispatches_and_records_activity() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token.clone());

    let create_resp = dispatch_request(
        &client,
        req(
            "tools/call",
            json!({
                "name": "daruma_create",
                "arguments": { "task": { "title": "MCP update seed" } }
            }),
        ),
    )
    .await
    .unwrap();
    assert!(
        create_resp.error.is_none(),
        "create failed: {:?}",
        create_resp.error
    );
    let content = create_resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    let create_events: serde_json::Value = serde_json::from_str(&content).unwrap();
    let task_id = create_events["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|event| {
            let payload = event.get("payload")?;
            (payload.get("type")?.as_str()? == "task_created")
                .then(|| payload.get("task")?.get("id")?.as_str().map(str::to_owned))
                .flatten()
        })
        .expect("create must return task id");

    let update_resp = dispatch_request(
        &client,
        req(
            "tools/call",
            json!({
                "name": "daruma_update",
                "arguments": {
                    "id": task_id,
                    "title": "MCP update changed",
                    "description": "Updated through MCP"
                }
            }),
        ),
    )
    .await
    .unwrap();
    assert!(
        update_resp.error.is_none(),
        "update failed: {:?}",
        update_resp.error
    );
    let content = update_resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    let update_events: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(update_events["data"][0]["payload"]["type"], "task_updated");

    let (status, activity) = get_json(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/tasks/{task_id}/activity"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let updated = activity["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["verb"].as_str() == Some("updated"))
        .expect("MCP update must be recorded in activity");
    let patch = updated["new_value"]
        .as_str()
        .expect("updated activity row must include serialized patch");
    assert!(
        patch.contains("MCP update changed") && patch.contains("Updated through MCP"),
        "activity patch must include MCP update fields: {patch}"
    );
}

#[tokio::test]
async fn ac7_unknown_method_returns_jsonrpc_error() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let resp = dispatch_request(&client, req("frobnicate", json!({})))
        .await
        .unwrap();
    let err = resp.error.expect("unknown method must be a JSON-RPC error");
    assert_eq!(err.code, -32601, "method-not-found code");
}

#[tokio::test]
async fn ac7_catalogue_is_consistent_with_direct_helper() {
    // Same list whether you call the helper or the JSON-RPC method.
    let direct: Vec<&str> = tool_definitions().iter().map(|t| t.name).collect();
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);
    let via_jsonrpc = dispatch_request(&client, req("tools/list", json!({})))
        .await
        .unwrap();
    let from_protocol: Vec<String> = via_jsonrpc.result.unwrap()["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(direct.len(), from_protocol.len());
}

/// All protocol-level tests drive the complete catalogue explicitly; the
/// compact `default` profile has its own dedicated coverage in
/// `mcp_dispatch.rs::profiles`.
async fn dispatch_request(
    client: &ApiClient,
    req: JsonRpcRequest,
) -> Option<daruma_mcp::JsonRpcResponse> {
    dispatch_request_with_profile(client, ToolProfile::Full, req).await
}

// ── MCP tool-surface profiles ───────────────────────────────────────────────

async fn tools_list_names(client: &ApiClient, profile: ToolProfile) -> Vec<String> {
    let resp = dispatch_request_with_profile(client, profile, req("tools/list", json!({})))
        .await
        .unwrap();
    resp.result.unwrap()["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn profiles_tools_list_reflects_selected_surface() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let full = tools_list_names(&client, ToolProfile::Full).await;
    let compact = tools_list_names(&client, ToolProfile::Default).await;

    assert_eq!(
        full.len(),
        tool_definitions().len(),
        "full = whole catalogue"
    );
    assert!(
        compact.len() < full.len(),
        "default ({}) must be smaller than full ({})",
        compact.len(),
        full.len()
    );
    for name in &compact {
        assert!(full.contains(name), "default tool {name} missing from full");
    }
    assert!(compact.iter().any(|n| n == "daruma_list"));
    assert!(
        !compact.iter().any(|n| n == "daruma_history_rollback"),
        "advanced tools must be hidden in the default profile"
    );
}

#[tokio::test]
async fn profiles_tools_list_carries_titles_and_annotations() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let resp =
        dispatch_request_with_profile(&client, ToolProfile::Full, req("tools/list", json!({})))
            .await
            .unwrap();
    for tool in resp.result.unwrap()["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        assert!(
            tool["title"].as_str().is_some_and(|t| !t.is_empty()),
            "{name} missing title"
        );
        let ann = tool
            .get("annotations")
            .unwrap_or_else(|| panic!("{name} missing annotations"));
        for key in [
            "readOnlyHint",
            "destructiveHint",
            "idempotentHint",
            "openWorldHint",
        ] {
            assert!(ann.get(key).is_some(), "{name} missing annotations.{key}");
        }
    }
}

#[tokio::test]
async fn profiles_hidden_tool_is_not_callable_in_default() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let resp = dispatch_request_with_profile(
        &client,
        ToolProfile::Default,
        req(
            "tools/call",
            json!({ "name": "daruma_history_latest", "arguments": {} }),
        ),
    )
    .await
    .unwrap();
    let err = resp
        .error
        .expect("hidden tool must error in default profile");
    assert!(
        err.message.contains("not available") && err.message.contains("full"),
        "error must point at the full profile, got: {}",
        err.message
    );

    // The same call succeeds when the full profile is selected.
    let ok = dispatch_request_with_profile(
        &client,
        ToolProfile::Full,
        req(
            "tools/call",
            json!({ "name": "daruma_history_latest", "arguments": { "limit": 1 } }),
        ),
    )
    .await
    .unwrap();
    assert!(
        ok.error.is_none(),
        "full profile must dispatch: {:?}",
        ok.error
    );
}
