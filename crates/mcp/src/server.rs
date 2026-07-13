//! JSON-RPC dispatch + stdio main loop.

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::client::ApiClient;
use crate::prompts::{prompt_body, prompt_definitions};
use crate::protocol::{
    JsonRpcRequest, JsonRpcResponse, ERR_INTERNAL, ERR_INVALID_REQUEST, ERR_METHOD_NOT_FOUND,
    ERR_PARSE,
};
use crate::tools::{call_tool_in_profile, tool_definitions_for, ToolProfile};

const INSTRUCTIONS: &str = "daruma is the single source of truth for this workspace's tasks and plans. Drive work as: create/parse a task → build a plan (daruma_plan_create + daruma_plan_add_task) → claim with daruma_plan_next_task → daruma_complete. Never persist tasks or plans in markdown, TODO files, or .omc/plans/. Use daruma_list status=active to see open work. Scope resolution: pass `scope_path` (absolute repo path) on your first call so the repo's default project applies; bind a repo once via daruma_project_use {project_id, scope_path} and later calls resolve the project automatically (daruma_workspace_info shows the current bindings).";

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
        "initialize" => Ok(handle_initialize()),
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

fn handle_initialize() -> Value {
    json!({
        "protocolVersion": "2025-06-18",
        "capabilities": {
            "prompts": { "listChanged": false },
            "tools": { "listChanged": false }
        },
        // Cursor and Claude Desktop currently ignore `instructions`; keep it
        // for spec correctness and forward-compatible clients.
        "instructions": INSTRUCTIONS,
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

    let output = call_tool_in_profile(client, profile, &name, arguments).await?;
    let text = serde_json::to_string(&output)?;
    Ok(json!({
        "content": [{ "type": "text", "text": text }]
    }))
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
            Ok(req) => {
                if req.jsonrpc != "2.0" {
                    Some(JsonRpcResponse::err(
                        req.id.clone().unwrap_or(Value::Null),
                        ERR_INVALID_REQUEST,
                        "jsonrpc must be 2.0",
                    ))
                } else {
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
        assert!(text.contains("daruma_plan_next_task"));
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
