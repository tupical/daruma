//! Low-level OpenAI Responses API client.
//!
//! `build_request_body` is `pub(crate)` and tested without network I/O.
//! All JSON is built with `serde_json::json!` — no string concatenation.

use serde_json::{json, Value};
use tracing::debug;

use crate::{config::AiConfig, error::AiError};

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

    /// Send a request to `POST {base_url}/responses` and parse the output list.
    pub async fn respond(&self, req: ResponseRequest) -> Result<Vec<ResponseOutput>, AiError> {
        let body = build_request_body(&self.config.model, &req);
        debug!(url = %self.config.responses_url(), "sending responses request");

        let resp = self
            .http
            .post(self.config.responses_url())
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
        parse_outputs(&json)
    }
}

// ── Request builder (pure, testable) ─────────────────────────────────────────

/// Build the Responses API request body as a `serde_json::Value`.
///
/// This function is `pub(crate)` so it can be unit-tested without networking.
pub(crate) fn build_request_body(model: &str, req: &ResponseRequest) -> Value {
    let mut obj = json!({
        "model": model,
        "input": req.input,
    });

    if !req.tools.is_empty() {
        obj["tools"] = Value::Array(req.tools.clone());
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

    #[test]
    fn build_body_minimal() {
        let req = make_req("hello", vec![], None);
        let body = build_request_body("gpt-4.1", &req);
        assert_eq!(body["model"], "gpt-4.1");
        assert_eq!(body["input"], "hello");
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn build_body_with_tools_and_choice() {
        let tool = json!({"type": "function", "name": "do_thing"});
        let req = make_req("prompt", vec![tool.clone()], Some("auto"));
        let body = build_request_body("gpt-4.1", &req);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["name"], "do_thing");
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
}
