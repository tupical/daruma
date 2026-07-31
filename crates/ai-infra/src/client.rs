//! Low-level OpenAI Responses API client.
//!
//! `build_request_body` is `pub(crate)` and tested without network I/O.
//! All JSON is built with `serde_json::json!` — no string concatenation.

use serde_json::{json, Value};
use tracing::debug;

use crate::{config::AiConfig, config::ApiProtocol, error::AiError};

// ── Public types ──────────────────────────────────────────────────────────────

/// Input parameters for a single Responses API call.
pub struct ResponseRequest {
    /// `input` field — a plain string or an array of message objects.
    pub input: Value,
    /// Tool schemas to advertise (may be empty).
    pub tools: Vec<Value>,
    /// Optional `tool_choice` value (`"auto"`, `"required"`, `"none"`).
    pub tool_choice: Option<String>,
}

/// A single item extracted from the `output` array of a Responses API reply.
#[derive(Debug)]
pub enum ResponseOutput {
    /// A text message from the assistant.
    Message(String),
    /// A function call the model wants to make.
    ToolCall(ToolCall),
}

/// One function call produced by the model.
#[derive(Debug)]
pub struct ToolCall {
    /// The function name as registered in the tool schema.
    pub name: String,
    /// Raw JSON string containing the function arguments.
    pub arguments: String,
}

// ── Client ────────────────────────────────────────────────────────────────────

/// Async wrapper around the OpenAI Responses API.
///
/// Clone cheaply — the inner [`reqwest::Client`] is Arc-backed.
#[derive(Clone, Debug)]
pub struct OpenAiClient {
    http: reqwest::Client,
    config: AiConfig,
}

impl OpenAiClient {
    /// Build a client from the given config. Reuses a single connection pool.
    pub fn new(config: AiConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            config,
        }
    }

    /// Build a client with caller-owned transport settings (timeouts, DNS pinning).
    pub fn with_http_client(config: AiConfig, http: reqwest::Client) -> Self {
        Self { http, config }
    }

    /// Send a request through the configured protocol and parse the output list.
    pub async fn respond(&self, req: ResponseRequest) -> Result<Vec<ResponseOutput>, AiError> {
        let (url, body) = match self.config.api_protocol {
            ApiProtocol::Responses => (
                self.config.responses_url(),
                build_request_body(&self.config, &req),
            ),
            ApiProtocol::ChatCompletions => (
                self.config.chat_completions_url(),
                build_chat_request_body(&self.config, &req),
            ),
        };
        debug!(%url, "sending AI request");

        let resp = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let message = resp.text().await.unwrap_or_default();
            return Err(AiError::Api { status, message });
        }

        let json: Value = resp.json().await?;
        match self.config.api_protocol {
            ApiProtocol::Responses => parse_outputs(&json),
            ApiProtocol::ChatCompletions => parse_chat_outputs(&json),
        }
    }
}

// ── Request builder (pure, testable) ─────────────────────────────────────────

/// Default `max_output_tokens` when [`AiConfig`] does not set one. Keeps the
/// proxy billers' cost forecast (and thus the balance reserve) small while
/// leaving room for structured replies and any hidden reasoning tokens.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 2000;

/// Build the Responses API request body as a `serde_json::Value`.
///
/// This function is `pub(crate)` so it can be unit-tested without networking.
pub(crate) fn build_request_body(config: &AiConfig, req: &ResponseRequest) -> Value {
    let mut obj = json!({
        "model": config.model,
        "input": req.input,
        "max_output_tokens": config.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
    });

    if !req.tools.is_empty() {
        obj["tools"] = Value::Array(req.tools.clone());
    }

    if let Some(tc) = &req.tool_choice {
        obj["tool_choice"] = Value::String(tc.clone());
    }

    obj
}

/// Build a Chat Completions request body from the provider-neutral request.
pub(crate) fn build_chat_request_body(config: &AiConfig, req: &ResponseRequest) -> Value {
    let mut obj = json!({
        "model": config.model,
        "messages": [{"role": "user", "content": req.input}],
        "max_tokens": config.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
    });

    if !req.tools.is_empty() {
        obj["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|tool| {
                    let mut function = serde_json::Map::new();
                    for field in ["name", "description", "parameters"] {
                        if let Some(value) = tool.get(field) {
                            function.insert(field.into(), value.clone());
                        }
                    }
                    json!({"type": "function", "function": function})
                })
                .collect(),
        );
    }

    if let Some(tc) = &req.tool_choice {
        obj["tool_choice"] = Value::String(tc.clone());
    }

    obj
}

// ── Response parser (pure) ────────────────────────────────────────────────────

fn parse_outputs(json: &Value) -> Result<Vec<ResponseOutput>, AiError> {
    let items = json["output"]
        .as_array()
        .ok_or_else(|| AiError::ParseFailed("response missing 'output' array".into()))?;

    let mut results = Vec::new();

    for item in items {
        match item["type"].as_str() {
            Some("message") => {
                if let Some(content) = item["content"].as_array() {
                    for part in content {
                        if part["type"] == "output_text" {
                            if let Some(text) = part["text"].as_str() {
                                results.push(ResponseOutput::Message(text.to_owned()));
                            }
                        }
                    }
                }
            }
            Some("function_call") => {
                let name = item["name"].as_str().unwrap_or("").to_owned();
                let arguments = item["arguments"].as_str().unwrap_or("{}").to_owned();
                results.push(ResponseOutput::ToolCall(ToolCall { name, arguments }));
            }
            _ => {
                // Unknown output type — skip gracefully.
            }
        }
    }

    Ok(results)
}

fn parse_chat_outputs(json: &Value) -> Result<Vec<ResponseOutput>, AiError> {
    let choice = json["choices"]
        .as_array()
        .and_then(|choices| choices.first())
        .ok_or_else(|| AiError::ParseFailed("response missing 'choices[0]'".into()))?;

    if choice["finish_reason"] == "length" {
        return Err(AiError::ParseFailed(
            "chat completion exhausted its token budget; increase max_output_tokens".into(),
        ));
    }

    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| AiError::ParseFailed("response missing 'choices[0].message'".into()))?;
    let mut results = Vec::new();

    if let Some(content) = message.get("content").and_then(Value::as_str) {
        results.push(ResponseOutput::Message(content.to_owned()));
    }

    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for tool_call in tool_calls {
            let function = &tool_call["function"];
            let name = function["name"]
                .as_str()
                .ok_or_else(|| AiError::ParseFailed("tool call missing function.name".into()))?;
            let arguments = function["arguments"].as_str().ok_or_else(|| {
                AiError::ParseFailed("tool call function.arguments must be a JSON string".into())
            })?;
            serde_json::from_str::<Value>(arguments).map_err(|error| {
                AiError::ParseFailed(format!(
                    "invalid tool call function.arguments JSON: {error}"
                ))
            })?;
            results.push(ResponseOutput::ToolCall(ToolCall {
                name: name.to_owned(),
                arguments: arguments.to_owned(),
            }));
        }
    }

    if results.is_empty() {
        return Err(AiError::ParseFailed(
            "chat response has neither content nor tool_calls".into(),
        ));
    }

    Ok(results)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_req(input: &str, tools: Vec<Value>, tool_choice: Option<&str>) -> ResponseRequest {
        ResponseRequest {
            input: Value::String(input.into()),
            tools,
            tool_choice: tool_choice.map(Into::into),
        }
    }

    fn make_cfg(max_output_tokens: Option<u32>) -> AiConfig {
        AiConfig {
            api_key: "sk-test".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4.1".into(),
            api_protocol: ApiProtocol::Responses,
            max_output_tokens,
        }
    }

    #[test]
    fn build_body_minimal() {
        let req = make_req("hello", vec![], None);
        let body = build_request_body(&make_cfg(None), &req);
        assert_eq!(body["model"], "gpt-4.1");
        assert_eq!(body["input"], "hello");
        assert_eq!(body["max_output_tokens"], DEFAULT_MAX_OUTPUT_TOKENS);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn build_body_with_tools_and_choice() {
        let tool = json!({"type": "function", "name": "do_thing"});
        let req = make_req("prompt", vec![tool.clone()], Some("auto"));
        let body = build_request_body(&make_cfg(Some(256)), &req);
        assert_eq!(body["max_output_tokens"], 256);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["name"], "do_thing");
    }

    #[test]
    fn build_chat_body_converts_tools_and_uses_default_max_tokens() {
        let tool = json!({
            "type": "function",
            "name": "report_status",
            "description": "Report status",
            "parameters": {"type": "object"}
        });
        let req = make_req("prompt", vec![tool], Some("required"));
        let body = build_chat_request_body(&make_cfg(None), &req);

        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": "prompt"}])
        );
        assert_eq!(body["max_tokens"], DEFAULT_MAX_OUTPUT_TOKENS);
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["tools"][0]["function"]["name"], "report_status");
        assert_eq!(
            body["tools"][0]["function"]["parameters"],
            json!({"type": "object"})
        );
    }

    #[test]
    fn parse_outputs_message() {
        let json = json!({
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "Hello!"}]
            }]
        });
        let out = parse_outputs(&json).unwrap();
        assert!(matches!(&out[0], ResponseOutput::Message(t) if t == "Hello!"));
    }

    #[test]
    fn parse_outputs_function_call() {
        let json = json!({
            "output": [{
                "type": "function_call",
                "name": "create_task",
                "arguments": "{\"title\":\"Buy milk\"}"
            }]
        });
        let out = parse_outputs(&json).unwrap();
        assert!(matches!(
            &out[0],
            ResponseOutput::ToolCall(tc) if tc.name == "create_task"
        ));
    }

    #[test]
    fn parse_outputs_missing_array_is_error() {
        let json = json!({"id": "resp_123"});
        assert!(parse_outputs(&json).is_err());
    }

    #[test]
    fn parse_chat_content_ignores_reasoning_and_extra_fields() {
        let json = json!({
            "model": "kimi-k3",
            "kv_transfer_params": {},
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "content": "Hello!",
                    "reasoning_content": "private reasoning"
                }
            }]
        });

        let out = parse_chat_outputs(&json).unwrap();
        assert!(matches!(&out[..], [ResponseOutput::Message(text)] if text == "Hello!"));
    }

    #[test]
    fn parse_chat_tool_call_accepts_null_content_and_json_string_arguments() {
        let json = json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "report_status:0",
                        "type": "function",
                        "function": {
                            "name": "report_status",
                            "arguments": "{\"ok\":true}"
                        }
                    }]
                }
            }]
        });

        let out = parse_chat_outputs(&json).unwrap();
        assert!(matches!(
            &out[..],
            [ResponseOutput::ToolCall(call)]
                if call.name == "report_status"
                    && serde_json::from_str::<Value>(&call.arguments).unwrap() == json!({"ok": true})
        ));

        let empty = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"content": null, "tool_calls": []}
            }]
        });
        assert!(parse_chat_outputs(&empty).is_err());
    }

    #[test]
    fn parse_chat_length_names_token_budget_setting() {
        let json = json!({
            "choices": [{
                "finish_reason": "length",
                "message": {"content": null, "tool_calls": []}
            }]
        });

        let error = parse_chat_outputs(&json).unwrap_err().to_string();
        assert!(error.contains("max_output_tokens"));
    }
}
