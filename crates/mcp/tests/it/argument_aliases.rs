//! Plan 01a06cfc, task 050114f3 — the id-alias and tool-error contracts as
//! an MCP client sees them: JSON-RPC `tools/call` through
//! [`dispatch_request_with_profile`], the entry the stdio loop uses.
//!
//! - the catalogue advertises an id alias on exactly the tools listed in
//!   [`ALIASED`], and on each of them neither name is `required`;
//! - a call with the canonical name and a call with the alias send the same
//!   request and return the same result;
//! - every tool failure (missing, wrong-kind, conflicting or non-string id,
//!   an HTTP rejection) is a `CallToolResult` with `isError: true` and the
//!   reason as text, never a JSON-RPC error, and an id failure sends nothing.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use axum::{body::Body, extract::Request, routing::any, Router};
use daruma_mcp::{
    dispatch_request_with_profile, tool_definitions_for, ApiClient, JsonRpcRequest,
    JsonRpcResponse, ToolProfile,
};
use serde_json::{json, Value};

type Captured = Arc<Mutex<Vec<(String, String, Value)>>>;

const AGENT: &str = "019e0000-0000-7000-8000-000000000001";

/// Tools whose schema lists an id alias: (tool, schema key, alias, the other
/// arguments of a valid call, the id value).
const ALIASED: [(&str, &str, &str, &str, &str); 11] = [
    ("daruma_get", "id", "task_id", "{}", "tsk_1"),
    (
        "daruma_update",
        "id",
        "task_id",
        r#"{"title":"t"}"#,
        "tsk_1",
    ),
    (
        "daruma_set_status",
        "id",
        "task_id",
        r#"{"status":"todo"}"#,
        "tsk_1",
    ),
    ("daruma_complete", "id", "task_id", "{}", "tsk_1"),
    (
        "daruma_comment",
        "task_id",
        "id",
        r#"{"body":"b"}"#,
        "tsk_1",
    ),
    ("daruma_relations", "task_id", "id", "{}", "tsk_1"),
    (
        "daruma_claim",
        "task_id",
        "id",
        r#"{"agent_id":"019e0000-0000-7000-8000-000000000001"}"#,
        "tsk_1",
    ),
    (
        "daruma_release",
        "task_id",
        "id",
        r#"{"agent_id":"019e0000-0000-7000-8000-000000000001"}"#,
        "tsk_1",
    ),
    ("daruma_plan_get", "id", "plan_id", "{}", "pln_1"),
    ("daruma_plan_progress", "plan_id", "id", "{}", "pln_1"),
    ("daruma_plan_drain_next", "plan_id", "id", "{}", "pln_1"),
];

/// Stub daruma API: records (method, path, body) and answers `status` with
/// `reply` (lists get `[]` on success).
async fn serve(
    captured: Captured,
    status: StatusCode,
    reply: Value,
) -> (ApiClient, tokio::task::JoinHandle<()>) {
    let router = Router::new().fallback(any(move |req: Request<Body>| {
        let captured = captured.clone();
        let reply = reply.clone();
        async move {
            let method = req.method().to_string();
            let path = req.uri().path_and_query().unwrap().as_str().to_string();
            let list = path.starts_with("/v1/tasks?") || path.starts_with("/v1/plans?");
            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .unwrap();
            let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            captured.lock().unwrap().push((method, path, body));
            let reply = if list && status.is_success() {
                json!([])
            } else {
                reply
            };
            (status, axum::Json(reply))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (ApiClient::new(base, "test-token"), handle)
}

fn tools_call(name: &str, arguments: Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(7)),
        method: "tools/call".to_string(),
        params: Some(json!({ "name": name, "arguments": arguments })),
    }
}

/// One `tools/call` against a stub answering `status`/`reply`: the JSON-RPC
/// response and every request the tool sent.
async fn rpc(
    tool: &str,
    arguments: Value,
    status: StatusCode,
    reply: Value,
) -> (JsonRpcResponse, Vec<(String, String, Value)>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (client, handle) = serve(captured.clone(), status, reply).await;
    let response =
        dispatch_request_with_profile(&client, ToolProfile::Default, tools_call(tool, arguments))
            .await
            .expect("tools/call with an id gets a response");
    handle.abort();
    let mut sent = captured.lock().unwrap().clone();
    for (_, _, body) in &mut sent {
        // Each client mints its own command actor id.
        if let Some(actor) = body.get_mut("actor").and_then(Value::as_object_mut) {
            actor.remove("id");
        }
    }
    (response, sent)
}

/// Text of an `isError` tool result; panics on a JSON-RPC error or success.
fn tool_error_text(tool: &str, response: &JsonRpcResponse) -> String {
    assert_eq!(response.id, json!(7), "{tool}: id echoed");
    assert!(
        response.error.is_none(),
        "{tool}: a tool failure must not be a JSON-RPC error: {:?}",
        response.error
    );
    let result = response.result.as_ref().expect("result");
    assert_eq!(result["isError"], true, "{tool}: {result}");
    assert_eq!(result["content"][0]["type"], "text", "{tool}: {result}");
    result["content"][0]["text"].as_str().unwrap().to_string()
}

fn args_with(base: &str, name: &str, id: &str) -> Value {
    let mut args: Value = serde_json::from_str(base).unwrap();
    args[name] = json!(id);
    args
}

/// The id names of one kind a schema can carry side by side.
fn alias_pair(properties: &serde_json::Map<String, Value>) -> Option<(&'static str, &'static str)> {
    ["task_id", "plan_id"]
        .into_iter()
        .find(|name| properties.contains_key(*name) && properties.contains_key("id"))
        .map(|name| ("id", name))
}

#[test]
fn catalogue_advertises_aliases_on_exactly_the_aliased_tools() {
    let expected: BTreeSet<&str> = ALIASED.iter().map(|case| case.0).collect();
    let mut advertised = BTreeSet::new();
    for tool in tool_definitions_for(ToolProfile::Full) {
        let Some(properties) = tool.input_schema["properties"].as_object() else {
            continue;
        };
        let Some((id, kind)) = alias_pair(properties) else {
            continue;
        };
        let required = tool.input_schema["required"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if required.contains(&json!(id)) || required.contains(&json!(kind)) {
            // Two names, one of them required: `id` names another object.
            continue;
        }
        for name in [id, kind] {
            assert_eq!(
                properties[name]["type"], "string",
                "{}.{name} is an id string",
                tool.name
            );
        }
        advertised.insert(tool.name);
    }
    assert_eq!(advertised, expected);

    let default: BTreeSet<&str> = tool_definitions_for(ToolProfile::Default)
        .into_iter()
        .map(|t| t.name)
        .collect();
    for tool in &expected {
        assert!(default.contains(tool), "{tool} is in the default profile");
    }
}

#[tokio::test]
async fn key_and_alias_send_the_same_request_and_return_the_same_result() {
    let reply = json!({"id": "echo", "status": "todo", "items": []});
    for (tool, key, alias, base, id) in ALIASED {
        let (by_key, sent_by_key) = rpc(
            tool,
            args_with(base, key, id),
            StatusCode::OK,
            reply.clone(),
        )
        .await;
        let (by_alias, sent_by_alias) = rpc(
            tool,
            args_with(base, alias, id),
            StatusCode::OK,
            reply.clone(),
        )
        .await;

        for (name, response) in [(key, &by_key), (alias, &by_alias)] {
            assert!(response.error.is_none(), "{tool} {{{name}}}: {response:?}");
            let result = response.result.as_ref().unwrap();
            assert!(
                result.get("isError").is_none(),
                "{tool} {{{name}}}: a success carries no isError: {result}"
            );
        }
        assert!(!sent_by_key.is_empty(), "{tool} {{{key}}} sent nothing");
        assert!(
            sent_by_key
                .iter()
                .any(|(_, path, body)| path.contains(id) || body.to_string().contains(id)),
            "{tool} {{{key}}} did not carry {id}: {sent_by_key:?}"
        );
        assert_eq!(sent_by_key, sent_by_alias, "{tool}: {key} vs {alias}");
        assert_eq!(by_key.result, by_alias.result, "{tool}: {key} vs {alias}");
    }
}

#[tokio::test]
async fn missing_id_is_an_is_error_result_naming_both_names() {
    for (tool, key, alias, base, _) in ALIASED {
        let args: Value = serde_json::from_str(base).unwrap();
        let (response, sent) = rpc(tool, args, StatusCode::OK, json!({})).await;
        let text = tool_error_text(tool, &response);
        assert!(
            text.starts_with(&format!("`{key}` (string) is required")),
            "{tool}: {text}"
        );
        assert!(text.contains(&format!("`{alias}`")), "{tool}: {text}");
        assert!(sent.is_empty(), "{tool} sent {sent:?}");
    }
}

#[tokio::test]
async fn bad_ids_are_is_error_results_and_send_nothing() {
    let cases = [
        (
            "daruma_get",
            json!({"id": "pln_1"}),
            "`id` = \"pln_1\" is a plan id, but this tool needs a task id",
        ),
        (
            "daruma_plan_get",
            json!({"plan_id": "tsk_1"}),
            "`plan_id` = \"tsk_1\" is a task id, but this tool needs a plan id",
        ),
        (
            "daruma_set_status",
            json!({"id": "tsk_1", "task_id": "tsk_2", "status": "todo"}),
            "conflicting task ids: `id` = \"tsk_1\", `task_id` = \"tsk_2\"",
        ),
        (
            "daruma_plan_get",
            json!({"id": "pln_1", "plan_id": "pln_2"}),
            "conflicting plan ids: `id` = \"pln_1\", `plan_id` = \"pln_2\"",
        ),
        (
            "daruma_comment",
            json!({"task_id": 42, "body": "b"}),
            "`task_id` must be a string id",
        ),
        (
            "daruma_comment",
            json!({"plan_id": "pln_1", "body": "b"}),
            "got `plan_id`, which names a plan, not a task",
        ),
    ];
    for (tool, args, needle) in cases {
        let (response, sent) = rpc(tool, args.clone(), StatusCode::OK, json!({})).await;
        let text = tool_error_text(tool, &response);
        assert!(text.contains(needle), "{tool} {args}: {text}");
        assert!(sent.is_empty(), "{tool} {args} sent {sent:?}");
    }
}

#[tokio::test]
async fn generic_id_beside_the_explicit_name_does_not_conflict() {
    // `task_id` is the comment tool's own name; a generic `id` next to it
    // may name something else and is ignored, not a conflict.
    let (response, sent) = rpc(
        "daruma_comment",
        json!({"task_id": "tsk_1", "id": "cmt_9", "body": "b"}),
        StatusCode::OK,
        json!({"id": "cmt_1"}),
    )
    .await;
    assert!(response.error.is_none(), "{response:?}");
    assert!(response.result.unwrap().get("isError").is_none());
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].1, "/v1/tasks/tsk_1/comments");
}

#[tokio::test]
async fn http_rejection_is_an_is_error_result_with_the_server_reason() {
    let (response, sent) = rpc(
        "daruma_set_status",
        json!({"task_id": "tsk_1", "status": "done", "agent_id": AGENT}),
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({"error": "lifecycle_gate: evidence required"}),
    )
    .await;
    let text = tool_error_text("daruma_set_status", &response);
    assert!(text.contains("422"), "{text}");
    assert!(text.contains("lifecycle_gate: evidence required"), "{text}");
    assert_eq!(sent.len(), 1, "one rejected write, no retry: {sent:?}");
    assert_eq!(sent[0].1, "/v1/commands");
    assert_eq!(sent[0].2["command"]["id"], "tsk_1", "{sent:?}");
}
