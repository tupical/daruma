//! JSON-RPC dispatch + stdio main loop.

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::client::ApiClient;
use crate::protocol::{
    JsonRpcRequest, JsonRpcResponse, ERR_INTERNAL, ERR_INVALID_REQUEST, ERR_METHOD_NOT_FOUND,
    ERR_PARSE,
};
use crate::tools::{call_tool_in_profile, tool_definitions_for, ToolProfile};

/// Dispatch a single JSON-RPC request using the profile resolved from
/// `TASKAGENT_MCP_PROFILE` (unset → `default`). Returns `Ok(None)` for
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
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "taskagent-mcp", "version": env!("CARGO_PKG_VERSION") }
    })
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
/// resolved from `TASKAGENT_MCP_PROFILE` (unset → `default`).
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
