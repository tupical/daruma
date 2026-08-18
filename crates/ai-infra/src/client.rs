//! Low-level OpenAI Responses API client.
//!
//! `build_request_body` is `pub(crate)` and tested without network I/O.
//! All JSON is built with `serde_json::json!` — no string concatenation.

use serde_json::{json, Value};
use std::time::Duration;
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

/// How long to wait for the TCP+TLS handshake before giving up.
///
/// The kernel's own ceiling here is `tcp_syn_retries`, typically two minutes of
/// silent retrying. Nothing upstream wants to wait that long to learn a provider
/// is unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on a whole request, handshake to last byte.
///
/// Well above any legitimate reasoning time for a single tool call, and far
/// enough below "wait forever" to bound the damage when a provider accepts a
/// request and never answers. That is not hypothetical: production recorded a
/// call sitting on a TCP-healthy connection for a full 300 seconds with nothing
/// coming back, which no transport setting can fix. This is the ceiling that
/// actually applies now that `tcp_user_timeout` no longer cuts calls off early,
/// so it doubles as the cap on how long one wedged call can stall an audit pass.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Keepalive probe interval on idle sockets.
///
/// A request awaiting a slow model looks exactly like an idle connection to a
/// NAT or load balancer, which is how such a connection gets dropped mid-answer;
/// probes keep the flow alive and, if the peer really is gone, surface it as a
/// prompt error rather than a stalled read.
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);

/// Drop pooled connections well before typical middlebox idle limits, so a
/// request is not handed a socket that has already been discarded upstream.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

impl OpenAiClient {
    /// Build a client from the given config. Reuses a single connection pool.
    pub fn new(config: AiConfig) -> Self {
        // Не удалять: тестовые бинарники не запускают main, а reqwest с
        // `rustls-no-provider` паникует, если CryptoProvider::get_default() пуст.
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let timeout = config
            .request_timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or(REQUEST_TIMEOUT);
        Self {
            http: Self::http_client_builder()
                .timeout(timeout)
                .tcp_user_timeout(timeout)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            config,
        }
    }

    /// The transport settings [`OpenAiClient::new`] applies.
    ///
    /// Exposed so callers that must build their own client — DNS pinning for an
    /// SSRF guard, say — start from these rather than from a bare
    /// `reqwest::Client::new()`, which has no timeout, no connect timeout and no
    /// keepalive whatsoever.
    pub fn http_client_builder() -> reqwest::ClientBuilder {
        // Не удалять: публичный builder вызывают в обход new; reqwest с
        // `rustls-no-provider` читает только CryptoProvider::get_default().
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .tcp_keepalive(TCP_KEEPALIVE)
            // `reqwest` defaults this to 30 seconds, which quietly caps how long
            // any request may wait: `TCP_USER_TIMEOUT` makes the kernel abort the
            // connection with `ETIMEDOUT` once data goes unacknowledged that
            // long, and an unanswered keepalive probe counts. A provider that
            // stays silent while its model thinks therefore had its connection
            // killed mid-answer at ~32s — measured in production at 32348,
            // 32102 and 32263 ms, a spread far too tight for packet loss.
            //
            // The ceiling on a slow model belongs to the request timeout, not to
            // a TCP-level abort a full order of magnitude below it, so this is
            // aligned with `REQUEST_TIMEOUT`. Keepalive still runs, so a peer
            // that is genuinely gone is still detected — just not mistaken for
            // one that is merely thinking.
            .tcp_user_timeout(REQUEST_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
    }

    /// Build a client with caller-owned transport settings (timeouts, DNS pinning).
    pub fn with_http_client(config: AiConfig, http: reqwest::Client) -> Self {
        Self { http, config }
    }

    /// Send a request through the configured protocol and parse the output list.
    pub async fn respond(&self, req: ResponseRequest) -> Result<Vec<ResponseOutput>, AiError> {
        let (url, body) = endpoint_and_body(&self.config, &req)?;
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
        parse_by_protocol(self.config.api_protocol, &json)
    }
}

/// Pick the endpoint and the matching request body for the configured protocol.
///
/// Split out of [`OpenAiClient::respond`] purely so it is reachable from a unit
/// test without a network: this pairing *is* the fix for the outage where every
/// workspace posted Responses bodies to a Chat-Completions-only provider. Two
/// crossed arms here — right URL, wrong body — reproduce that outage exactly
/// while every other test in this file stays green.
pub(crate) fn endpoint_and_body(
    config: &AiConfig,
    req: &ResponseRequest,
) -> Result<(String, Value), AiError> {
    match config.api_protocol {
        ApiProtocol::Responses => Ok((config.responses_url(), build_request_body(config, req))),
        ApiProtocol::ChatCompletions => Ok((
            config.chat_completions_url(),
            build_chat_request_body(config, req)?,
        )),
    }
}

/// Parse a provider reply with the parser that matches the protocol it was
/// requested over. Split out for the same reason as [`endpoint_and_body`].
pub(crate) fn parse_by_protocol(
    protocol: ApiProtocol,
    json: &Value,
) -> Result<Vec<ResponseOutput>, AiError> {
    match protocol {
        ApiProtocol::Responses => parse_outputs(json),
        ApiProtocol::ChatCompletions => parse_chat_outputs(json),
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

    // Responses API nests it where Chat Completions takes a flat field. Omitted
    // entirely when unset: a provider that does not know this parameter rejects
    // the whole request rather than ignoring it, so it stays opt-in.
    if let Some(effort) = &config.reasoning_effort {
        obj["reasoning"] = json!({ "effort": effort });
    }

    obj
}

/// Build a Chat Completions request body from the provider-neutral request.
pub(crate) fn build_chat_request_body(
    config: &AiConfig,
    req: &ResponseRequest,
) -> Result<Value, AiError> {
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
                    if tool.get("type").and_then(Value::as_str) != Some("function") {
                        return Err(AiError::Config(format!(
                            "chat_completions does not support built-in tool {}",
                            tool.get("type")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown")
                        )));
                    }
                    let mut function = serde_json::Map::new();
                    for field in ["name", "description", "parameters"] {
                        if let Some(value) = tool.get(field) {
                            function.insert(field.into(), value.clone());
                        }
                    }
                    if function.get("name").and_then(Value::as_str).is_none() {
                        return Err(AiError::Config(
                            "chat_completions function tool requires name".into(),
                        ));
                    }
                    Ok(json!({"type": "function", "function": function}))
                })
                .collect::<Result<Vec<_>, AiError>>()?,
        );
    }

    if let Some(tc) = &req.tool_choice {
        obj["tool_choice"] = Value::String(tc.clone());
    }

    if let Some(effort) = &config.reasoning_effort {
        obj["reasoning_effort"] = Value::String(effort.clone());
    }

    Ok(obj)
}

// ── Response parser (pure) ────────────────────────────────────────────────────

fn parse_outputs(json: &Value) -> Result<Vec<ResponseOutput>, AiError> {
    // Ответ, обрезанный по лимиту токенов, приходит с валидным конвертом и
    // ПОЛОВИНОЙ tool-call'а: `arguments` — оборванная JSON-строка. Без этой
    // проверки такой ответ уезжал дальше и падал у вызывающего невнятным
    // `serialization error: EOF while parsing a string at line 1 column N`.
    // Chat Completions ветка уже ловит это по `finish_reason == "length"`.
    if json["status"] == "incomplete" {
        let reason = json["incomplete_details"]["reason"]
            .as_str()
            .unwrap_or("unknown");
        return Err(AiError::ParseFailed(format!(
            "response incomplete ({reason}); increase max_output_tokens              or ask for fewer items per call"
        )));
    }

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
            reasoning_effort: None,
            max_output_tokens,
            request_timeout_seconds: None,
        }
    }

    #[test]
    fn reasoning_effort_uses_each_protocol_own_shape_and_is_omitted_when_unset() {
        // Two different schemas for one setting: Responses nests it under
        // `reasoning.effort`, Chat Completions takes a flat `reasoning_effort`.
        // Sending the wrong shape is silently ignored by a lenient gateway,
        // which would leave the model on its default — for Kimi K3 that default
        // is `max`, and at `max` it spends the whole token budget thinking and
        // never emits the tool call.
        let req = make_req("hi", vec![], None);

        let mut cfg = make_cfg(None);
        assert!(build_request_body(&cfg, &req).get("reasoning").is_none());
        assert!(build_chat_request_body(&cfg, &req)
            .unwrap()
            .get("reasoning_effort")
            .is_none());

        cfg.reasoning_effort = Some("low".into());
        assert_eq!(build_request_body(&cfg, &req)["reasoning"]["effort"], "low");
        assert!(build_request_body(&cfg, &req)
            .get("reasoning_effort")
            .is_none());
        assert_eq!(
            build_chat_request_body(&cfg, &req).unwrap()["reasoning_effort"],
            "low"
        );
        assert!(build_chat_request_body(&cfg, &req)
            .unwrap()
            .get("reasoning")
            .is_none());
    }

    #[test]
    fn protocol_selects_matching_endpoint_and_body_shape() {
        // The outage this shipped to fix was exactly a crossed pair: a
        // Responses-shaped body posted to a Chat-Completions-only provider.
        // Assert URL *and* body marker together — checking either alone lets a
        // swapped arm through.
        let req = make_req("hi", vec![], None);

        let mut cfg = make_cfg(None);
        cfg.api_protocol = ApiProtocol::Responses;
        let (url, body) = endpoint_and_body(&cfg, &req).unwrap();
        assert!(url.ends_with("/responses"), "{url}");
        assert_eq!(body["input"], "hi");
        assert!(body.get("messages").is_none(), "chat body on responses");

        cfg.api_protocol = ApiProtocol::ChatCompletions;
        let (url, body) = endpoint_and_body(&cfg, &req).unwrap();
        assert!(url.ends_with("/chat/completions"), "{url}");
        assert_eq!(body["messages"][0]["content"], "hi");
        assert!(body.get("input").is_none(), "responses body on chat");
    }

    #[test]
    fn protocol_selects_matching_reply_parser() {
        // Same crossed-arm risk on the way back: each parser must be fed only
        // the wire shape its protocol actually returns.
        let responses_json = json!({"output": [{"type": "message", "content": [
            {"type": "output_text", "text": "from responses"}
        ]}]});
        let chat_json = json!({"choices": [{"finish_reason": "stop", "message": {
            "role": "assistant", "content": "from chat"
        }}]});

        let out = parse_by_protocol(ApiProtocol::Responses, &responses_json).unwrap();
        assert!(matches!(&out[..], [ResponseOutput::Message(t)] if t == "from responses"));
        let out = parse_by_protocol(ApiProtocol::ChatCompletions, &chat_json).unwrap();
        assert!(matches!(&out[..], [ResponseOutput::Message(t)] if t == "from chat"));

        // Crossed the other way, each parser must fail rather than silently
        // return nothing — a quiet empty list is what makes such a bug survive.
        assert!(parse_by_protocol(ApiProtocol::ChatCompletions, &responses_json).is_err());
        assert!(parse_by_protocol(ApiProtocol::Responses, &chat_json).is_err());
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
        let body = build_chat_request_body(&make_cfg(None), &req).unwrap();

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
    fn chat_rejects_responses_only_tools() {
        let req = make_req(
            "search",
            vec![json!({"type": "web_search"})],
            Some("required"),
        );
        assert!(build_chat_request_body(&make_cfg(None), &req).is_err());
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

    /// Регрессия: обрезанный по лимиту токенов ответ несёт валидный конверт и
    /// оборванный `arguments`. Раньше он проходил парсер и падал у вызывающего
    /// как «serialization error: EOF while parsing a string».
    #[test]
    fn parse_outputs_rejects_truncated_response_with_actionable_message() {
        let json = json!({
            "id": "resp_123",
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{
                "type": "function_call",
                "name": "report_duplicates",
                "arguments": "{\"verdicts\":[{\"pair_index\":1,\"reason\":\"обрыв"
            }]
        });

        let err = parse_outputs(&json).unwrap_err().to_string();
        assert!(err.contains("max_output_tokens"), "{err}");
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
