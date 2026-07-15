//! MCP tool integration tests for relation tools (§3.2 W3.2 / AC-9).
//!
//! Tests:
//!   catalogue_includes_relation_tools — AC-9: all 3 relation tool names present
//!   link_unlink_roundtrip             — daruma_link → relation_id; daruma_unlink → ok
//!   relations_read_returns_five_groups — daruma_relations returns 5-group projection

use daruma_mcp::{
    dispatch_request_with_profile, tool_definitions, ApiClient, JsonRpcRequest, ToolProfile,
};
use serde_json::json;

mod common;
use common::{spawn_server, test_app};

async fn spawn_daruma_inline() -> (std::net::SocketAddr, String) {
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

/// Materialize a task via MCP (plan-only intake) and return its task id string.
async fn create_task_via_mcp(client: &ApiClient, title: &str) -> String {
    // MaterializePlan needs a project host; seed one per task (cheap in tests).
    let proj = dispatch_request(
        client,
        req(
            "tools/call",
            json!({
                "name": "daruma_project_create",
                "arguments": { "title": format!("proj for {title}") }
            }),
        ),
    )
    .await
    .unwrap();
    assert!(proj.error.is_none(), "project create failed: {:?}", proj.error);
    let proj_text = proj.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    let proj_body: serde_json::Value = serde_json::from_str(&proj_text).unwrap();
    let pid = proj_body["project_id"]
        .as_str()
        .expect("project_id in response")
        .to_owned();

    let resp = dispatch_request(
        client,
        req(
            "tools/call",
            json!({
                "name": "daruma_plan_materialize",
                "arguments": {
                    "plan": { "title": format!("plan for {title}"), "project_id": pid },
                    "tasks": [ { "title": title } ]
                }
            }),
        ),
    )
    .await
    .unwrap();
    assert!(resp.error.is_none(), "materialize failed: {:?}", resp.error);
    let content_text = resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    let body: serde_json::Value = serde_json::from_str(&content_text).unwrap();
    body["data"]
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
        .expect("task_created event with task.id")
}

// ── AC-9: catalogue includes all 3 relation tool names ───────────────────────

/// AC-9: tool_definitions() must include daruma_link, daruma_unlink,
/// daruma_relations.
#[tokio::test]
async fn catalogue_includes_relation_tools() {
    let names: Vec<&str> = tool_definitions().iter().map(|t| t.name).collect();
    for required in ["daruma_link", "daruma_unlink", "daruma_relations"] {
        assert!(
            names.contains(&required),
            "AC-9: missing relation tool: {required}"
        );
    }
    assert!(
        names.len() >= 44,
        "AC-9: catalogue must have ≥44 tools (got {})",
        names.len()
    );
}

// ── link / unlink roundtrip ───────────────────────────────────────────────────

/// daruma_link creates a relation and returns a relation_id;
/// daruma_unlink deletes it and returns success.
#[tokio::test]
async fn link_unlink_roundtrip() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let from = create_task_via_mcp(&client, "Blocker").await;
    let to = create_task_via_mcp(&client, "Blocked").await;

    // Link
    let link_resp = dispatch_request(
        &client,
        req(
            "tools/call",
            json!({
                "name": "daruma_link",
                "arguments": { "from": from, "to": to, "kind": "blocks" }
            }),
        ),
    )
    .await
    .unwrap();
    assert!(
        link_resp.error.is_none(),
        "daruma_link failed: {:?}",
        link_resp.error
    );
    let link_text = link_resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    let link_body: serde_json::Value = serde_json::from_str(&link_text).unwrap();
    assert_eq!(link_body["success"], true, "link must succeed: {link_body}");
    let relation_id = link_body["data"]["relation_id"]
        .as_str()
        .expect("data.relation_id must be a string")
        .to_owned();
    assert!(!relation_id.is_empty(), "relation_id must not be empty");

    // Unlink
    let unlink_resp = dispatch_request(
        &client,
        req(
            "tools/call",
            json!({
                "name": "daruma_unlink",
                "arguments": { "relation_id": relation_id }
            }),
        ),
    )
    .await
    .unwrap();
    assert!(
        unlink_resp.error.is_none(),
        "daruma_unlink failed: {:?}",
        unlink_resp.error
    );
    let unlink_text = unlink_resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    let unlink_body: serde_json::Value = serde_json::from_str(&unlink_text).unwrap();
    assert_eq!(
        unlink_body["success"], true,
        "unlink must succeed: {unlink_body}"
    );
}

// ── relations read returns five groups ────────────────────────────────────────

/// Create relations of all 3 kinds from task A, then daruma_relations on A.
/// blocks/relates_to/duplicates must be non-empty; blocked_by/duplicated_by empty.
#[tokio::test]
async fn relations_read_returns_five_groups() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let a = create_task_via_mcp(&client, "Task A").await;
    let b = create_task_via_mcp(&client, "Task B").await;
    let c = create_task_via_mcp(&client, "Task C").await;
    let d = create_task_via_mcp(&client, "Task D").await;

    // A blocks B
    let r1 = dispatch_request(
        &client,
        req(
            "tools/call",
            json!({
                "name": "daruma_link",
                "arguments": { "from": a, "to": b, "kind": "blocks" }
            }),
        ),
    )
    .await
    .unwrap();
    assert!(r1.error.is_none(), "link A->B failed: {:?}", r1.error);

    // A relates_to C
    let r2 = dispatch_request(
        &client,
        req(
            "tools/call",
            json!({
                "name": "daruma_link",
                "arguments": { "from": a, "to": c, "kind": "relates_to" }
            }),
        ),
    )
    .await
    .unwrap();
    assert!(r2.error.is_none(), "link A->C failed: {:?}", r2.error);

    // A duplicates D
    let r3 = dispatch_request(
        &client,
        req(
            "tools/call",
            json!({
                "name": "daruma_link",
                "arguments": { "from": a, "to": d, "kind": "duplicates" }
            }),
        ),
    )
    .await
    .unwrap();
    assert!(r3.error.is_none(), "link A->D failed: {:?}", r3.error);

    // Read relations for A
    let rel_resp = dispatch_request(
        &client,
        req(
            "tools/call",
            json!({
                "name": "daruma_relations",
                "arguments": { "task_id": a }
            }),
        ),
    )
    .await
    .unwrap();
    assert!(
        rel_resp.error.is_none(),
        "daruma_relations failed: {:?}",
        rel_resp.error
    );
    let rel_text = rel_resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    let rel_body: serde_json::Value = serde_json::from_str(&rel_text).unwrap();

    assert_eq!(
        rel_body["blocks"].as_array().unwrap().len(),
        1,
        "blocks must have 1: {rel_body}"
    );
    assert_eq!(
        rel_body["blocked_by"].as_array().unwrap().len(),
        0,
        "blocked_by must be empty: {rel_body}"
    );
    assert_eq!(
        rel_body["relates_to"].as_array().unwrap().len(),
        1,
        "relates_to must have 1: {rel_body}"
    );
    assert_eq!(
        rel_body["duplicates"].as_array().unwrap().len(),
        1,
        "duplicates must have 1: {rel_body}"
    );
    assert_eq!(
        rel_body["duplicated_by"].as_array().unwrap().len(),
        0,
        "duplicated_by must be empty: {rel_body}"
    );
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
