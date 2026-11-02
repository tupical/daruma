//! JSON-RPC dispatch + stdio main loop.

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::client::ApiClient;
use crate::open_tasks::{
    fetch_open_tasks, open_tasks_line, open_tasks_to_meta, MAX_INSTRUCTIONS_CHARS,
    OPEN_TASKS_META_KEY,
};
use crate::prompts::{prompt_body, prompt_definitions};
use crate::protocol::{
    JsonRpcRequest, JsonRpcResponse, ERR_INTERNAL, ERR_INVALID_REQUEST, ERR_METHOD_NOT_FOUND,
    ERR_PARSE,
};
use crate::tools::{call_tool_in_profile, tool_definitions_for, ToolProfile};

const INSTRUCTIONS: &str = "daruma is the single source of truth for this workspace's tasks and plans. Intake is plan-only (no direct task creation): decided work → daruma_plan_materialize; raw ideas → the idea pipeline, see Routing. Status follows writes: claim or intent/progress comment → in_progress; outcome comment → in_review; daruma_complete → done. Never persist tasks or plans in markdown, TODO or local plan files. Use daruma_list status=active to see open work. Id arguments: `id` and the kind's own name (`task_id`, `plan_id`, `run_id`, …) are interchangeable. Scope: pass `scope_path` (absolute repo path) on the first call; bind a repo once via daruma_project_use {project_id, scope_path}.";

/// Dispatch a single JSON-RPC request using the profile resolved from
/// `DARUMA_MCP_PROFILE` (unset → `default`). Returns `Ok(None)` for
/// notifications (no `id` present, no response expected).
pub async fn dispatch_request(client: &ApiClient, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
    dispatch_request_with_profile(client, ToolProfile::from_env(), req).await
}

/// Dispatch a single JSON-RPC request against an explicit tool profile.
/// `tools/list` advertises only the profile's tools and `tools/call`
/// refuses tools the profile hides.
pub async fn dispatch_request_with_profile(
    client: &ApiClient,
    profile: ToolProfile,
    req: JsonRpcRequest,
) -> Option<JsonRpcResponse> {
    let id = req.id.clone();

    // Notifications (id absent) don't get a reply.
    let id_value = id?;

    let result = match req.method.as_str() {
        "initialize" => Ok(handle_initialize(client).await),
        "prompts/list" => Ok(handle_prompts_list()),
        "prompts/get" => handle_prompts_get(req.params.unwrap_or(Value::Null)),
        "tools/list" => Ok(handle_tools_list(profile)),
        "tools/call" => handle_tools_call(client, profile, req.params.unwrap_or(Value::Null)).await,
        "ping" => Ok(json!({})),
        other => {
            return Some(JsonRpcResponse::err(
                id_value,
                ERR_METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            ))
        }
    };

    match result {
        Ok(v) => Some(JsonRpcResponse::ok(id_value, v)),
        Err(e) => Some(JsonRpcResponse::err(id_value, ERR_INTERNAL, e.to_string())),
    }
}

/// Static rules plus, when this agent holds open tasks, one line naming them
/// (`crate::open_tasks`) within [`MAX_INSTRUCTIONS_CHARS`]; the same tasks
/// ride `_meta` for a gateway that appends its own text.
async fn handle_initialize(client: &ApiClient) -> Value {
    let open = fetch_open_tasks(client).await;
    let budget = MAX_INSTRUCTIONS_CHARS.saturating_sub(INSTRUCTIONS.chars().count() + 2);
    let instructions = match open_tasks_line(&open, budget) {
        Some(line) => format!("{INSTRUCTIONS}\n\n{line}"),
        None => INSTRUCTIONS.to_string(),
    };
    let mut result = initialize_result(&instructions);
    if !open.is_empty() {
        result["_meta"] = json!({ OPEN_TASKS_META_KEY: open_tasks_to_meta(&open) });
    }
    result
}

fn initialize_result(instructions: &str) -> Value {
    json!({
        "protocolVersion": "2025-06-18",
        "capabilities": {
            "prompts": { "listChanged": false },
            "tools": { "listChanged": false }
        },
        // Cursor and Claude Desktop currently ignore `instructions`; keep it
        // for spec correctness and forward-compatible clients.
        "instructions": instructions,
        "serverInfo": { "name": "daruma-mcp", "version": env!("CARGO_PKG_VERSION") }
    })
}

fn handle_prompts_list() -> Value {
    json!({ "prompts": prompt_definitions() })
}

fn handle_prompts_get(params: Value) -> anyhow::Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("`name` (string) is required"))?;
    let description = prompt_definitions()
        .into_iter()
        .find(|prompt| prompt.name == name)
        .map(|prompt| prompt.description)
        .ok_or_else(|| anyhow::anyhow!("unknown prompt: {name}"))?;
    let text = prompt_body(name).ok_or_else(|| anyhow::anyhow!("unknown prompt: {name}"))?;

    Ok(json!({
        "description": description,
        "messages": [{
            "role": "user",
            "content": { "type": "text", "text": text }
        }]
    }))
}

fn handle_tools_list(profile: ToolProfile) -> Value {
    let tools = tool_definitions_for(profile);
    json!({ "tools": tools })
}

async fn handle_tools_call(
    client: &ApiClient,
    profile: ToolProfile,
    params: Value,
) -> anyhow::Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("`name` (string) is required"))?
        .to_string();
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));

    // A failing tool is a tool result, not a protocol failure (MCP
    // 2025-06-18 "Error Handling"): the model must see the message to
    // self-correct, and clients drop JSON-RPC errors before the model.
    let output = match call_tool_in_profile(client, profile, &name, arguments).await {
        Ok(output) => output,
        Err(e) => return Ok(tool_error_result(&e.to_string())),
    };
    let text = serde_json::to_string(&output)?;
    Ok(json!({
        "content": [{ "type": "text", "text": text }]
    }))
}

/// `CallToolResult` for a tool that ran and failed.
fn tool_error_result(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true
    })
}

/// Read newline-delimited JSON-RPC frames from stdin, dispatch them, and
/// write responses to stdout. Blocks until EOF. The tool profile is
/// resolved from `DARUMA_MCP_PROFILE` (unset → `default`).
pub async fn run_stdio(client: ApiClient) -> anyhow::Result<()> {
    run_stdio_with_profile(client, ToolProfile::from_env()).await
}

/// [`run_stdio`] with an explicit tool profile (e.g. from a CLI flag).
pub async fn run_stdio_with_profile(client: ApiClient, profile: ToolProfile) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Result<JsonRpcRequest, _> = serde_json::from_str(&line);
        let response = match parsed {
            Ok(mut req) => {
                if req.jsonrpc != "2.0" {
                    Some(JsonRpcResponse::err(
                        req.id.clone().unwrap_or(Value::Null),
                        ERR_INVALID_REQUEST,
                        "jsonrpc must be 2.0",
                    ))
                } else {
                    enrich_local_session_request(&mut req);
                    dispatch_request_with_profile(&client, profile, req).await
                }
            }
            Err(e) => Some(JsonRpcResponse::err(
                Value::Null,
                ERR_PARSE,
                format!("parse error: {e}"),
            )),
        };

        if let Some(resp) = response {
            let json = serde_json::to_string(&resp)?;
            stdout.write_all(json.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

/// This enrichment is deliberately outside the shared HTTP dispatch path.
fn enrich_local_session_request(req: &mut JsonRpcRequest) {
    use crate::session_metadata::{
        git_work_context, merge_defaults, KEY_GIT_WORK_CONTEXT, KEY_WORKSPACE_PATH,
    };
    if req.method != "tools/call" {
        return;
    }
    let Some(params) = req.params.as_mut().and_then(Value::as_object_mut) else {
        return;
    };
    if params.get("name").and_then(Value::as_str) != Some("daruma_session_start") {
        return;
    }
    let Some(args) = params
        .entry("arguments")
        .or_insert_with(|| json!({}))
        .as_object_mut()
    else {
        return;
    };
    if args.get("metadata").is_some_and(|value| !value.is_object()) {
        return;
    }
    let mut metadata = merge_defaults(args.get("metadata").cloned().unwrap_or_else(|| json!({})));
    if metadata.get(KEY_GIT_WORK_CONTEXT).is_none() {
        if let Some(path) = metadata[KEY_WORKSPACE_PATH].as_str() {
            let mr = std::env::var("DARUMA_MERGE_REQUEST_ID").ok();
            if let Some(context) = git_work_context(std::path::Path::new(path), mr.as_deref()) {
                metadata[KEY_GIT_WORK_CONTEXT] = context;
            }
        }
    }
    args.insert("metadata".into(), metadata);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::JsonRpcRequest;

    fn request(method: &str, params: Option<Value>) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: method.to_string(),
            params,
        }
    }

    #[test]
    fn local_enrichment_preserves_explicit_remote_context_and_malformed_requests() {
        for params in [
            None,
            Some(json!(null)),
            Some(json!({"name":"daruma_session_start", "arguments":42})),
        ] {
            let mut req = request("tools/call", params);
            let original = serde_json::to_value(&req).unwrap();
            enrich_local_session_request(&mut req);
            assert_eq!(serde_json::to_value(req).unwrap(), original);
        }
        let context = json!({"worktree_path":"/remote/repo", "head_sha":"caller-snapshot"});
        let mut req = request(
            "tools/call",
            Some(json!({
                "name":"daruma_session_start", "arguments":{"metadata":{"git_work_context":context}}
            })),
        );
        enrich_local_session_request(&mut req);
        assert_eq!(
            req.params.unwrap()["arguments"]["metadata"]["git_work_context"],
            context
        );
    }

    #[tokio::test]
    async fn initialize_advertises_prompts_and_instructions() {
        let client = ApiClient::new("http://127.0.0.1:1", "test-token");
        let response = dispatch_request_with_profile(
            &client,
            ToolProfile::Default,
            request("initialize", None),
        )
        .await
        .unwrap();
        let result = response.result.unwrap();

        assert!(result["capabilities"]["prompts"].is_object());
        assert!(result["instructions"].as_str().unwrap_or_default().len() > 0);
    }

    #[tokio::test]
    async fn prompts_list_returns_builtin_prompts() {
        let client = ApiClient::new("http://127.0.0.1:1", "test-token");
        let response = dispatch_request_with_profile(
            &client,
            ToolProfile::Default,
            request("prompts/list", None),
        )
        .await
        .unwrap();
        let result = response.result.unwrap();
        let prompts = result["prompts"].as_array().unwrap();
        let names: Vec<&str> = prompts
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();

        assert_eq!(
            names,
            vec!["daruma-tasks", "daruma-plan", "daruma-next", "daruma-mine"]
        );
    }

    #[tokio::test]
    async fn prompts_get_returns_prompt_body() {
        let client = ApiClient::new("http://127.0.0.1:1", "test-token");
        let response = dispatch_request_with_profile(
            &client,
            ToolProfile::Default,
            request(
                "prompts/get",
                Some(json!({ "name": "daruma-next", "arguments": {} })),
            ),
        )
        .await
        .unwrap();
        let result = response.result.unwrap();
        let text = result["messages"][0]["content"]["text"].as_str().unwrap();

        assert!(!text.is_empty());
        assert!(text.contains("daruma_plan_drain_next"));
    }

    #[tokio::test]
    async fn tool_failure_is_a_tool_result_with_is_error() {
        // Port 1 refuses the connection: the tool runs and fails.
        let client = ApiClient::new("http://127.0.0.1:1", "test-token");
        let response = dispatch_request_with_profile(
            &client,
            ToolProfile::Default,
            request(
                "tools/call",
                Some(json!({ "name": "daruma_healthz", "arguments": {} })),
            ),
        )
        .await
        .unwrap();

        assert!(response.error.is_none(), "{:?}", response.error);
        let result = response.result.unwrap();
        assert_eq!(result["isError"], true, "{result}");
        assert_eq!(result["content"][0]["type"], "text");
        assert!(!result["content"][0]["text"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn hidden_and_unknown_tools_are_tool_errors() {
        let client = ApiClient::new("http://127.0.0.1:1", "test-token");
        for (name, needle) in [
            ("daruma_history_latest", "not available"),
            ("daruma_no_such_tool", "daruma_no_such_tool"),
        ] {
            let response = dispatch_request_with_profile(
                &client,
                ToolProfile::Default,
                request("tools/call", Some(json!({ "name": name }))),
            )
            .await
            .unwrap();
            assert!(response.error.is_none(), "{name}: {:?}", response.error);
            let result = response.result.unwrap();
            assert_eq!(result["isError"], true, "{name}: {result}");
            let text = result["content"][0]["text"].as_str().unwrap();
            assert!(text.contains(needle), "{name}: {text}");
        }
    }

    #[tokio::test]
    async fn tools_call_without_name_stays_a_protocol_error() {
        let client = ApiClient::new("http://127.0.0.1:1", "test-token");
        let response = dispatch_request_with_profile(
            &client,
            ToolProfile::Default,
            request("tools/call", Some(json!({ "arguments": {} }))),
        )
        .await
        .unwrap();

        assert!(response.result.is_none());
        assert!(response.error.unwrap().message.contains("name"));
    }

    #[tokio::test]
    async fn prompts_get_unknown_name_errors() {
        let client = ApiClient::new("http://127.0.0.1:1", "test-token");
        let response = dispatch_request_with_profile(
            &client,
            ToolProfile::Default,
            request("prompts/get", Some(json!({ "name": "nope" }))),
        )
        .await
        .unwrap();

        assert!(response.result.is_none());
        assert!(response.error.is_some());
    }
}
