//! Low-level client for OpenAI Responses, Chat Completions and Anthropic
//! Messages APIs.
//!
//! `build_request_body` is `pub(crate)` and tested without network I/O.
//! All JSON is built with `serde_json::json!` — no string concatenation.

use serde_json::{json, Value};
use std::time::Duration;
use tracing::{debug, info};

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
    /// Stable instructions shared by every call of one kind (review rules,
    /// project context). Anthropic gets them as a cached `system` block; the
    /// OpenAI protocols get them prepended to the input, see [`with_system`].
    pub system: Option<String>,
}

/// Separator between the stable system part and the per-call input wherever
/// the protocol has no dedicated place for the system part.
pub const SYSTEM_SEPARATOR: &str = "\n\n";

/// The input with the system part prepended — the request the OpenAI
/// protocols send. A string input stays a single string, so the request keeps
/// exactly the shape providers were verified with; the shared prefix still
/// comes first, where OpenAI's automatic prefix caching can reuse it. An array
/// input gets a leading system message instead.
pub(crate) fn with_system(req: &ResponseRequest) -> Value {
    match (req.system.as_deref().filter(|s| !s.is_empty()), &req.input) {
        (None, input) => input.clone(),
        (Some(system), Value::String(input)) => {
            Value::String(format!("{system}{SYSTEM_SEPARATOR}{input}"))
        }
        (Some(system), Value::Array(items)) => {
            let mut out = vec![json!({"role": "system", "content": system})];
            out.extend(items.iter().cloned());
            Value::Array(out)
        }
        (Some(_), other) => other.clone(),
    }
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
        // Resolved before sending: a reply nobody can read must not cost a
        // provider request.
        let parse = reply_parser(self.config.api_protocol)?;
        debug!(%url, "sending AI request");

        let mut request = self.http.post(url);
        for (name, value) in auth_headers(&self.config) {
            request = request.header(name, value);
        }
        let resp = request
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
        if self.config.api_protocol == ApiProtocol::AnthropicMessages {
            if let Some(usage) = anthropic_usage(&json) {
                info!(
                    model = %self.config.model,
                    input_tokens = usage.input_tokens,
                    cache_creation_input_tokens = usage.cache_creation_input_tokens,
                    cache_read_input_tokens = usage.cache_read_input_tokens,
                    output_tokens = usage.output_tokens,
                    "anthropic usage"
                );
            }
        }
        parse(&json)
    }
}

/// How long [`OpenAiClient::respond_batch`] waits for a Message Batch and how
/// often it asks.
///
/// Anthropic finishes most batches within the hour and guarantees only 24
/// hours, so the wait is bounded: a batch still running at the deadline is
/// cancelled and the caller gets an error it can fall back from. The default
/// suits an offline pass (scheduled audit) — nothing interactive should ever
/// wait on a batch.
#[derive(Clone, Copy, Debug)]
pub struct BatchWait {
    pub poll_interval: Duration,
    pub deadline: Duration,
}

impl Default for BatchWait {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(10),
            deadline: Duration::from_secs(15 * 60),
        }
    }
}

impl OpenAiClient {
    /// Send many independent one-turn requests as one Anthropic Message Batch
    /// (half the price of the same calls made one by one) and wait for them.
    ///
    /// The outer `Err` means the batch as a whole failed — not created, not
    /// finished by [`BatchWait::deadline`] (then it is cancelled), results
    /// unreadable — and nothing was answered. Otherwise item `i` is the reply
    /// to `reqs[i]`, each one parsed exactly as [`Self::respond`] parses it; a
    /// request the batch errored, cancelled or let expire is an `Err` of its
    /// own. Only `anthropic_messages` has batches: the other protocols get a
    /// configuration error before any request is sent.
    pub async fn respond_batch(
        &self,
        reqs: &[ResponseRequest],
        wait: BatchWait,
    ) -> Result<Vec<Result<Vec<ResponseOutput>, AiError>>, AiError> {
        if self.config.api_protocol != ApiProtocol::AnthropicMessages {
            return Err(AiError::Config(
                "message batches need api_protocol anthropic_messages".into(),
            ));
        }
        let body = build_anthropic_batch_body(&self.config, reqs)?;
        let batches_url = self.config.anthropic_batches_url();
        let created = self
            .anthropic_call(self.http.post(&batches_url).json(&body))
            .await?;
        let id = created["id"]
            .as_str()
            .ok_or_else(|| AiError::Batch(format!("created batch has no id: {created}")))?
            .to_owned();
        info!(batch_id = %id, requests = reqs.len(), model = %self.config.model, "anthropic batch created");

        let started = std::time::Instant::now();
        let mut batch = created;
        while batch["processing_status"] != "ended" {
            if started.elapsed() >= wait.deadline {
                // Best effort: an uncancelled batch still finishes and bills,
                // but the caller has already moved on without its answers.
                if let Err(error) = self
                    .anthropic_call(self.http.post(format!("{batches_url}/{id}/cancel")))
                    .await
                {
                    tracing::warn!(batch_id = %id, %error, "anthropic batch cancel failed");
                }
                return Err(AiError::Batch(format!(
                    "batch {id} not ended within {}s, cancelled",
                    wait.deadline.as_secs()
                )));
            }
            tokio::time::sleep(wait.poll_interval).await;
            batch = self
                .anthropic_call(self.http.get(format!("{batches_url}/{id}")))
                .await?;
        }

        let results_url = batch["results_url"]
            .as_str()
            .ok_or_else(|| AiError::Batch(format!("ended batch {id} has no results_url")))?;
        let mut request = self.http.get(results_url);
        for (name, value) in auth_headers(&self.config) {
            request = request.header(name, value);
        }
        let resp = request.send().await?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let message = resp.text().await.unwrap_or_default();
            return Err(AiError::Api { status, message });
        }
        let jsonl = resp.text().await?;
        let results = parse_anthropic_batch_results(&jsonl, reqs.len())?;
        for message in results.iter().flatten() {
            if let Some(usage) = anthropic_usage(message) {
                info!(
                    model = %self.config.model,
                    batch_id = %id,
                    input_tokens = usage.input_tokens,
                    cache_creation_input_tokens = usage.cache_creation_input_tokens,
                    cache_read_input_tokens = usage.cache_read_input_tokens,
                    output_tokens = usage.output_tokens,
                    "anthropic usage"
                );
            }
        }
        Ok(results
            .into_iter()
            .map(|result| result.and_then(|message| parse_anthropic_outputs(&message)))
            .collect())
    }

    pub(crate) fn config(&self) -> &AiConfig {
        &self.config
    }

    /// One authenticated JSON call to the Anthropic API; non-2xx is an error.
    async fn anthropic_call(&self, mut request: reqwest::RequestBuilder) -> Result<Value, AiError> {
        for (name, value) in auth_headers(&self.config) {
            request = request.header(name, value);
        }
        let resp = request.send().await?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let message = resp.text().await.unwrap_or_default();
            return Err(AiError::Api { status, message });
        }
        Ok(resp.json().await?)
    }
}

/// `custom_id` of request `i` in a batch. Anthropic returns results in any
/// order, so this is how a result finds its request.
fn batch_custom_id(i: usize) -> String {
    format!("req-{i}")
}

/// The Message Batch create body: request `i` carries [`batch_custom_id`]`(i)`
/// and exactly the body [`OpenAiClient::respond`] would send for it, cache
/// breakpoints included.
pub(crate) fn build_anthropic_batch_body(
    config: &AiConfig,
    reqs: &[ResponseRequest],
) -> Result<Value, AiError> {
    let requests = reqs
        .iter()
        .enumerate()
        .map(|(i, req)| {
            Ok(json!({
                "custom_id": batch_custom_id(i),
                "params": build_anthropic_request_body(config, req)?,
            }))
        })
        .collect::<Result<Vec<_>, AiError>>()?;
    Ok(json!({ "requests": requests }))
}

/// Split the results JSONL of an ended batch into one entry per request, in
/// request order: the `message` of a succeeded request, otherwise an error
/// naming what happened to it. A request the file does not mention is an error
/// too — it was never answered.
pub(crate) fn parse_anthropic_batch_results(
    jsonl: &str,
    n: usize,
) -> Result<Vec<Result<Value, AiError>>, AiError> {
    let mut out: Vec<Option<Result<Value, AiError>>> = (0..n).map(|_| None).collect();
    for line in jsonl.lines().filter(|line| !line.trim().is_empty()) {
        let item: Value = serde_json::from_str(line)?;
        let index = item["custom_id"]
            .as_str()
            .and_then(|id| id.strip_prefix("req-"))
            .and_then(|i| i.parse::<usize>().ok())
            .filter(|i| *i < n)
            .ok_or_else(|| AiError::Batch(format!("result for an unknown request: {line}")))?;
        let result = &item["result"];
        out[index] = Some(match result["type"].as_str() {
            Some("succeeded") => Ok(result["message"].clone()),
            Some("errored") => Err(AiError::Batch(format!(
                "request errored: {}",
                result["error"]
            ))),
            Some(other) => Err(AiError::Batch(format!("request {other}"))),
            None => Err(AiError::Batch(format!("result without type: {line}"))),
        });
    }
    Ok(out
        .into_iter()
        .map(|result| result.unwrap_or_else(|| Err(AiError::Batch("request has no result".into()))))
        .collect())
}

/// Token accounting of one Anthropic Messages reply, cache split included.
///
/// `input_tokens` counts only the uncached part of the prompt; the cached
/// prefix is billed separately as a write (`cache_creation_input_tokens`) or a
/// read (`cache_read_input_tokens`). Logged so the effect of
/// [`anthropic_cache_control`] can be measured per call.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct AnthropicUsage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
}

/// Read `usage` from an Anthropic reply. Missing counters — older or
/// Anthropic-compatible gateways omit the cache ones — read as zero.
pub(crate) fn anthropic_usage(json: &Value) -> Option<AnthropicUsage> {
    let usage = json.get("usage")?.as_object()?;
    let count = |field: &str| usage.get(field).and_then(Value::as_u64).unwrap_or(0);
    Some(AnthropicUsage {
        input_tokens: count("input_tokens"),
        cache_creation_input_tokens: count("cache_creation_input_tokens"),
        cache_read_input_tokens: count("cache_read_input_tokens"),
        output_tokens: count("output_tokens"),
    })
}

/// Anthropic API version pinned in the `anthropic-version` header.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Authentication headers for the configured protocol.
///
/// OpenAI-style providers take `Authorization: Bearer`; Anthropic rejects it
/// and wants `x-api-key` plus a pinned `anthropic-version` instead.
pub(crate) fn auth_headers(config: &AiConfig) -> Vec<(&'static str, String)> {
    match config.api_protocol {
        ApiProtocol::Responses | ApiProtocol::ChatCompletions => {
            vec![("Authorization", format!("Bearer {}", config.api_key))]
        }
        ApiProtocol::AnthropicMessages => vec![
            ("x-api-key", config.api_key.clone()),
            ("anthropic-version", ANTHROPIC_VERSION.to_owned()),
        ],
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
        ApiProtocol::AnthropicMessages => Ok((
            config.anthropic_messages_url(),
            build_anthropic_request_body(config, req)?,
        )),
    }
}

type ReplyParser = fn(&Value) -> Result<Vec<ResponseOutput>, AiError>;

/// The parser that matches the protocol a reply was requested over. Split out
/// for the same reason as [`endpoint_and_body`].
pub(crate) fn reply_parser(protocol: ApiProtocol) -> Result<ReplyParser, AiError> {
    match protocol {
        ApiProtocol::Responses => Ok(parse_outputs),
        ApiProtocol::ChatCompletions => Ok(parse_chat_outputs),
        ApiProtocol::AnthropicMessages => Ok(parse_anthropic_outputs),
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
        "input": with_system(req),
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
        "messages": [{"role": "user", "content": with_system(req)}],
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

/// Cache breakpoint Anthropic puts at the end of the prefix it should cache.
/// `ephemeral` is the default five-minute cache, refreshed on every hit.
pub(crate) fn anthropic_cache_control() -> Value {
    json!({"type": "ephemeral"})
}

/// Build an Anthropic Messages request body from the provider-neutral request.
///
/// Limits of this first iteration:
/// - `input` must be a plain string; it becomes the single user message. An
///   array of OpenAI-style message objects is rejected rather than nested into
///   an invalid `content`.
/// - `system`, when set, is sent as one text block of the `system` field.
/// - `reasoning_effort` is not sent: Anthropic has no such parameter, and
///   extended thinking needs its own token budget.
/// - `tool_choice` is omitted without tools — there is nothing to choose.
///
/// Prompt caching: Anthropic caches the prefix in the order tools → system →
/// messages. The last tool carries [`anthropic_cache_control`] (tool schemas
/// are identical for every call of one kind), and so does the system block
/// (review rules and project context shared by the calls of one run) — the
/// second breakpoint covers tools and system together. The user message is
/// deliberately not marked: it differs per call, and a cache write costs more
/// than plain input, so caching it would raise the price of every one-off
/// prompt. A prefix below the model's minimum cacheable length is simply not
/// cached; the request is still valid.
pub(crate) fn build_anthropic_request_body(
    config: &AiConfig,
    req: &ResponseRequest,
) -> Result<Value, AiError> {
    let Some(input) = req.input.as_str() else {
        return Err(AiError::Config(
            "anthropic_messages supports only a plain string input".into(),
        ));
    };
    let mut obj = json!({
        "model": config.model,
        "max_tokens": config.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        "messages": [{"role": "user", "content": input}],
    });
    if let Some(system) = req.system.as_deref().filter(|s| !s.is_empty()) {
        obj["system"] = json!([{
            "type": "text",
            "text": system,
            "cache_control": anthropic_cache_control(),
        }]);
    }

    if req.tools.is_empty() {
        return Ok(obj);
    }

    obj["tools"] = Value::Array(
        req.tools
            .iter()
            .map(|tool| {
                if tool.get("type").and_then(Value::as_str) != Some("function") {
                    return Err(AiError::Config(format!(
                        "anthropic_messages does not support built-in tool {}",
                        tool.get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                    )));
                }
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    return Err(AiError::Config(
                        "anthropic_messages function tool requires name".into(),
                    ));
                };
                let mut out = json!({
                    "name": name,
                    // Required by Anthropic; a parameterless function has none.
                    "input_schema": tool
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"})),
                });
                if let Some(description) = tool.get("description") {
                    out["description"] = description.clone();
                }
                Ok(out)
            })
            .collect::<Result<Vec<_>, AiError>>()?,
    );
    if let Some(last) = obj["tools"]
        .as_array_mut()
        .and_then(|tools| tools.last_mut())
    {
        last["cache_control"] = anthropic_cache_control();
    }

    if let Some(tc) = &req.tool_choice {
        obj["tool_choice"] = match tc.as_str() {
            "auto" => json!({"type": "auto"}),
            "required" => json!({"type": "any"}),
            "none" => json!({"type": "none"}),
            other => {
                return Err(AiError::Config(format!(
                    "anthropic_messages does not support tool_choice '{other}'"
                )))
            }
        };
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

fn parse_anthropic_outputs(json: &Value) -> Result<Vec<ResponseOutput>, AiError> {
    // Same truncation trap as the other protocols: a `max_tokens` stop leaves a
    // half-written `tool_use.input`, so name the setting instead of passing it on.
    if json["stop_reason"] == "max_tokens" {
        return Err(AiError::ParseFailed(
            "response incomplete (max_tokens); increase max_output_tokens \
             or ask for fewer items per call"
                .into(),
        ));
    }

    let blocks = json["content"]
        .as_array()
        .ok_or_else(|| AiError::ParseFailed("response missing 'content' array".into()))?;

    let mut results = Vec::new();

    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str() {
                    results.push(ResponseOutput::Message(text.to_owned()));
                }
            }
            Some("tool_use") => {
                let name = block["name"]
                    .as_str()
                    .ok_or_else(|| AiError::ParseFailed("tool_use block missing name".into()))?;
                // Anthropic sends `input` as an object; ToolCall carries the
                // OpenAI-style JSON string that callers `from_str`.
                let input = block
                    .get("input")
                    .filter(|input| input.is_object())
                    .ok_or_else(|| {
                        AiError::ParseFailed("tool_use block input must be a JSON object".into())
                    })?;
                results.push(ResponseOutput::ToolCall(ToolCall {
                    name: name.to_owned(),
                    arguments: input.to_string(),
                }));
            }
            _ => {
                // thinking, redacted_thinking and future block types carry
                // nothing callers consume.
            }
        }
    }

    if results.is_empty() {
        return Err(AiError::ParseFailed(
            "anthropic response has neither text nor tool_use content".into(),
        ));
    }

    Ok(results)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_by_protocol(
        protocol: ApiProtocol,
        json: &Value,
    ) -> Result<Vec<ResponseOutput>, AiError> {
        reply_parser(protocol)?(json)
    }

    fn make_req(input: &str, tools: Vec<Value>, tool_choice: Option<&str>) -> ResponseRequest {
        ResponseRequest {
            input: Value::String(input.into()),
            tools,
            tool_choice: tool_choice.map(Into::into),
            system: None,
        }
    }

    fn with_system_req(system: &str, input: &str, tools: Vec<Value>) -> ResponseRequest {
        ResponseRequest {
            system: Some(system.into()),
            ..make_req(input, tools, Some("required"))
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
    fn anthropic_messages_pairs_messages_url_with_messages_body() {
        let req = make_req("hi", vec![], None);
        let mut cfg = make_cfg(None);
        cfg.api_protocol = ApiProtocol::AnthropicMessages;
        cfg.base_url = "https://api.anthropic.com".into();
        let (url, body) = endpoint_and_body(&cfg, &req).unwrap();
        assert_eq!(url, "https://api.anthropic.com/v1/messages");
        assert_eq!(body["messages"], json!([{"role": "user", "content": "hi"}]));
        assert_eq!(body["max_tokens"], DEFAULT_MAX_OUTPUT_TOKENS);
        assert!(body.get("input").is_none(), "responses body on anthropic");
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn anthropic_messages_selects_messages_parser() {
        let anthropic_json = json!({"stop_reason": "end_turn", "content": [
            {"type": "text", "text": "from anthropic"}
        ]});
        let out = parse_by_protocol(ApiProtocol::AnthropicMessages, &anthropic_json).unwrap();
        assert!(matches!(&out[..], [ResponseOutput::Message(t)] if t == "from anthropic"));

        // Crossed arms must fail in both directions, not return an empty list.
        assert!(parse_by_protocol(ApiProtocol::Responses, &anthropic_json).is_err());
        assert!(parse_by_protocol(ApiProtocol::ChatCompletions, &anthropic_json).is_err());
        let responses_json = json!({"output": [{"type": "message", "content": [
            {"type": "output_text", "text": "from responses"}
        ]}]});
        let chat_json = json!({"choices": [{"finish_reason": "stop", "message": {
            "role": "assistant", "content": "from chat"
        }}]});
        assert!(parse_by_protocol(ApiProtocol::AnthropicMessages, &responses_json).is_err());
        assert!(parse_by_protocol(ApiProtocol::AnthropicMessages, &chat_json).is_err());
    }

    /// One-shot HTTP stub: accepts one request, answers `reply` as JSON and
    /// returns the raw request text from the join handle.
    fn one_shot_anthropic_stub(
        reply: Value,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if raw.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            let reply = reply.to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .unwrap();
            String::from_utf8(raw).unwrap()
        });
        (addr, server)
    }

    fn anthropic_stub_client(addr: std::net::SocketAddr) -> OpenAiClient {
        let mut cfg = make_cfg(None);
        cfg.api_protocol = ApiProtocol::AnthropicMessages;
        cfg.base_url = format!("http://{addr}");
        // The dev host proxies loopback through HTTP_PROXY; the stub must be hit directly.
        let http = OpenAiClient::http_client_builder()
            .no_proxy()
            .build()
            .unwrap();
        OpenAiClient::with_http_client(cfg, http)
    }

    #[tokio::test]
    async fn generate_object_with_system_sends_cached_system_block_over_anthropic() {
        use crate::AiProvider;

        let (addr, server) = one_shot_anthropic_stub(json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "report", "input": {"ok": true}}],
            "usage": {"input_tokens": 5, "cache_read_input_tokens": 1200, "output_tokens": 7}
        }));
        let tool = json!({"type": "function", "name": "report"});
        let args = anthropic_stub_client(addr)
            .generate_object_with_system("RULES".into(), "per-call".into(), vec![tool], "report")
            .await
            .unwrap();
        assert_eq!(args, json!({"ok": true}));

        let request = server.join().unwrap();
        let sent: Value =
            serde_json::from_str(&request[request.find("\r\n\r\n").unwrap() + 4..]).unwrap();
        assert_eq!(
            sent["system"],
            json!([{"type": "text", "text": "RULES", "cache_control": {"type": "ephemeral"}}]),
            "{sent}"
        );
        assert_eq!(
            sent["messages"],
            json!([{"role": "user", "content": "per-call"}]),
            "{sent}"
        );
    }

    #[tokio::test]
    async fn respond_over_anthropic_messages_round_trips_tool_use() {
        let (addr, server) = one_shot_anthropic_stub(json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "report_status", "input": {"ok": true}}],
            "usage": {"input_tokens": 5, "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 1200, "output_tokens": 7}
        }));

        let tool = json!({"type": "function", "name": "report_status",
            "parameters": {"type": "object", "properties": {"ok": {"type": "boolean"}}}});
        let out = anthropic_stub_client(addr)
            .respond(make_req("status?", vec![tool], Some("required")))
            .await
            .unwrap();

        let request = server.join().unwrap();
        let head = request.to_ascii_lowercase();
        assert!(request.starts_with("POST /v1/messages "), "{request}");
        assert!(head.contains("x-api-key:"), "{request}");
        assert!(
            head.contains(&format!("anthropic-version: {ANTHROPIC_VERSION}")),
            "{request}"
        );
        assert!(!head.contains("authorization:"), "{request}");
        let sent: Value =
            serde_json::from_str(&request[request.find("\r\n\r\n").unwrap() + 4..]).unwrap();
        assert_eq!(
            sent["tools"][0]["cache_control"],
            json!({"type": "ephemeral"}),
            "{sent}"
        );
        let [ResponseOutput::ToolCall(call)] = &out[..] else {
            panic!("unexpected outputs: {out:?}");
        };
        assert_eq!(call.name, "report_status");
        assert_eq!(
            serde_json::from_str::<Value>(&call.arguments).unwrap(),
            json!({"ok": true})
        );
    }

    #[test]
    fn auth_headers_follow_protocol() {
        let mut cfg = make_cfg(None);
        for protocol in [ApiProtocol::Responses, ApiProtocol::ChatCompletions] {
            cfg.api_protocol = protocol;
            assert_eq!(
                auth_headers(&cfg),
                vec![("Authorization", "Bearer sk-test".to_owned())]
            );
        }

        // Anthropic answers Bearer with 401; the key must travel only in
        // x-api-key.
        cfg.api_protocol = ApiProtocol::AnthropicMessages;
        assert_eq!(
            auth_headers(&cfg),
            vec![
                ("x-api-key", "sk-test".to_owned()),
                ("anthropic-version", "2023-06-01".to_owned()),
            ]
        );
    }

    #[test]
    fn build_anthropic_body_maps_tools_and_tool_choice() {
        let tool = json!({
            "type": "function",
            "name": "report_status",
            "description": "Report status",
            "parameters": {"type": "object", "properties": {"ok": {"type": "boolean"}}}
        });
        let req = make_req("prompt", vec![tool], Some("required"));
        let body = build_anthropic_request_body(&make_cfg(Some(256)), &req).unwrap();

        assert_eq!(body["model"], "gpt-4.1");
        assert_eq!(body["max_tokens"], 256);
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": "prompt"}])
        );
        assert!(body.get("system").is_none());
        assert_eq!(
            body["tools"],
            json!([{
                "name": "report_status",
                "description": "Report status",
                "input_schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}},
                "cache_control": {"type": "ephemeral"}
            }])
        );
        assert_eq!(body["tool_choice"], json!({"type": "any"}));

        for (choice, expected) in [("auto", "auto"), ("none", "none")] {
            let tool = json!({"type": "function", "name": "f"});
            let req = make_req("p", vec![tool], Some(choice));
            let body = build_anthropic_request_body(&make_cfg(None), &req).unwrap();
            assert_eq!(body["tool_choice"], json!({"type": expected}));
            assert_eq!(body["tools"][0]["input_schema"], json!({"type": "object"}));
        }
    }

    #[test]
    fn build_anthropic_body_caches_tool_prefix_but_not_the_prompt() {
        // One breakpoint after the last tool caches every tool schema; a mark on
        // an earlier tool would leave the rest of the stable prefix uncached.
        let tools = ["first", "second", "third"]
            .map(|name| json!({"type": "function", "name": name}))
            .to_vec();
        let body =
            build_anthropic_request_body(&make_cfg(None), &make_req("p", tools, Some("required")))
                .unwrap();
        let marks: Vec<_> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool.get("cache_control").cloned())
            .collect();
        assert_eq!(marks, [None, None, Some(json!({"type": "ephemeral"}))]);
        // The per-call prompt stays a plain string: marking it would pay the
        // cache-write premium on input that is never read back.
        assert_eq!(body["messages"], json!([{"role": "user", "content": "p"}]));
        assert!(body.get("cache_control").is_none(), "{body}");

        let body =
            build_anthropic_request_body(&make_cfg(None), &make_req("p", vec![], None)).unwrap();
        assert!(!body.to_string().contains("cache_control"), "{body}");
    }

    #[test]
    fn build_anthropic_body_caches_system_after_tools() {
        // Anthropic caches tools → system → messages; the breakpoint on the
        // system block makes the cached prefix cover the stable instructions,
        // not only the tool schemas. The per-call input stays uncached.
        let tools = vec![json!({"type": "function", "name": "report"})];
        let body = build_anthropic_request_body(
            &make_cfg(None),
            &with_system_req("RULES", "per-call", tools),
        )
        .unwrap();
        assert_eq!(
            body["system"],
            json!([{"type": "text", "text": "RULES", "cache_control": {"type": "ephemeral"}}])
        );
        assert_eq!(
            body["tools"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": "per-call"}])
        );

        // Without tools the system breakpoint alone still caches the rules.
        let body = build_anthropic_request_body(
            &make_cfg(None),
            &with_system_req("RULES", "per-call", vec![]),
        )
        .unwrap();
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );

        // An empty system part sends no empty block (Anthropic rejects it).
        let body =
            build_anthropic_request_body(&make_cfg(None), &with_system_req("", "per-call", vec![]))
                .unwrap();
        assert!(body.get("system").is_none(), "{body}");
    }

    #[test]
    fn openai_protocols_prepend_system_to_the_input() {
        // The OpenAI bodies keep the shape providers were verified with: one
        // string input / one user message, system text first.
        let req = with_system_req("RULES", "per-call", vec![]);
        let body = build_request_body(&make_cfg(None), &req);
        assert_eq!(body["input"], json!("RULES\n\nper-call"));
        assert!(body.get("instructions").is_none(), "{body}");
        let body = build_chat_request_body(&make_cfg(None), &req).unwrap();
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": "RULES\n\nper-call"}])
        );

        // Unset system leaves the bodies byte-identical to before.
        let plain = make_req("per-call", vec![], None);
        assert_eq!(
            build_request_body(&make_cfg(None), &plain)["input"],
            json!("per-call")
        );

        // An array input gets a leading system message instead.
        let req = ResponseRequest {
            input: json!([{"role": "user", "content": "p"}]),
            ..with_system_req("RULES", "", vec![])
        };
        assert_eq!(
            build_request_body(&make_cfg(None), &req)["input"],
            json!([{"role": "system", "content": "RULES"}, {"role": "user", "content": "p"}])
        );
    }

    #[test]
    fn anthropic_usage_reads_cache_split_and_defaults_missing_counters() {
        let json = json!({"usage": {
            "input_tokens": 12,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 1500,
            "output_tokens": 40
        }});
        assert_eq!(
            anthropic_usage(&json),
            Some(AnthropicUsage {
                input_tokens: 12,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1500,
                output_tokens: 40,
            })
        );
        // A compatible gateway without caching reports only the basic pair.
        assert_eq!(
            anthropic_usage(&json!({"usage": {"input_tokens": 5, "output_tokens": 7}})),
            Some(AnthropicUsage {
                input_tokens: 5,
                output_tokens: 7,
                ..Default::default()
            })
        );
        assert_eq!(anthropic_usage(&json!({"content": []})), None);
    }

    #[test]
    fn build_anthropic_body_omits_optional_fields_when_unset() {
        let mut cfg = make_cfg(None);
        cfg.reasoning_effort = Some("low".into());
        let body =
            build_anthropic_request_body(&cfg, &make_req("p", vec![], Some("auto"))).unwrap();
        assert_eq!(body["max_tokens"], DEFAULT_MAX_OUTPUT_TOKENS);
        for field in [
            "tools",
            "tool_choice",
            "reasoning",
            "reasoning_effort",
            "system",
        ] {
            assert!(body.get(field).is_none(), "{field}: {body}");
        }
    }

    #[test]
    fn build_anthropic_body_rejects_what_it_cannot_express() {
        let cfg = make_cfg(None);
        let function = json!({"type": "function", "name": "f"});
        let cases = [
            make_req("p", vec![json!({"type": "web_search"})], None),
            make_req("p", vec![json!({"type": "function"})], None),
            make_req("p", vec![function], Some("required_all")),
            ResponseRequest {
                input: json!([{"role": "user", "content": "p"}]),
                tools: vec![],
                tool_choice: None,
                system: None,
            },
        ];
        for req in cases {
            assert!(matches!(
                build_anthropic_request_body(&cfg, &req),
                Err(AiError::Config(_))
            ));
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

    #[test]
    fn parse_anthropic_text_blocks_skip_thinking() {
        let json = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "stop_reason": "end_turn",
            "content": [
                {"type": "thinking", "thinking": "private", "signature": "sig"},
                {"type": "text", "text": "Hello"},
                {"type": "text", "text": "world"}
            ],
            "usage": {"input_tokens": 3, "output_tokens": 2}
        });

        let out = parse_anthropic_outputs(&json).unwrap();
        assert!(matches!(
            &out[..],
            [ResponseOutput::Message(a), ResponseOutput::Message(b)] if a == "Hello" && b == "world"
        ));
    }

    #[test]
    fn parse_anthropic_tool_use_serializes_input_object_to_json_string() {
        let input = json!({"verdicts": [{"pair_index": 1, "reason": "дубль \"A\""}], "ok": true});
        let json = json!({
            "stop_reason": "tool_use",
            "content": [
                {"type": "text", "text": "Calling the tool."},
                {"type": "tool_use", "id": "toolu_1", "name": "report_duplicates", "input": input}
            ]
        });

        let out = parse_anthropic_outputs(&json).unwrap();
        let [ResponseOutput::Message(_), ResponseOutput::ToolCall(call)] = &out[..] else {
            panic!("unexpected outputs: {out:?}");
        };
        assert_eq!(call.name, "report_duplicates");
        assert_eq!(
            serde_json::from_str::<Value>(&call.arguments).unwrap(),
            input
        );
    }

    #[test]
    fn parse_anthropic_rejects_malformed_tool_use_and_empty_content() {
        let no_input = json!({"stop_reason": "tool_use", "content": [
            {"type": "tool_use", "id": "toolu_1", "name": "report_status"}
        ]});
        assert!(parse_anthropic_outputs(&no_input).is_err());

        let string_input = json!({"stop_reason": "tool_use", "content": [
            {"type": "tool_use", "id": "toolu_1", "name": "report_status", "input": "{}"}
        ]});
        assert!(parse_anthropic_outputs(&string_input).is_err());

        let empty = json!({"stop_reason": "end_turn", "content": []});
        assert!(parse_anthropic_outputs(&empty).is_err());
        assert!(parse_anthropic_outputs(&json!({"stop_reason": "end_turn"})).is_err());
    }

    #[test]
    fn parse_anthropic_max_tokens_names_token_budget_setting() {
        // A truncated reply still carries a well-formed (but partial) input
        // object; it must not reach the caller.
        let json = json!({
            "stop_reason": "max_tokens",
            "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "report_status", "input": {}}
            ]
        });

        let error = parse_anthropic_outputs(&json).unwrap_err();
        assert!(
            matches!(&error, AiError::ParseFailed(m) if m.contains("max_tokens") && m.contains("max_output_tokens")),
            "{error:?}"
        );
    }

    // ── Message Batches ──────────────────────────────────────────────────────

    /// Answers one connection per canned `(status, body)` in order and returns
    /// every raw request it read. `replies` gets the stub address so a reply
    /// can point back at it (`results_url`).
    fn scripted_anthropic_stub(
        replies: impl FnOnce(std::net::SocketAddr) -> Vec<(u16, String)>,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let replies = replies(addr);
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, reply) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if raw.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
                requests.push(String::from_utf8(raw).unwrap());
            }
            requests
        });
        (addr, server)
    }

    fn request_line(raw: &str) -> &str {
        raw.lines().next().unwrap_or_default()
    }

    fn request_body(raw: &str) -> Value {
        serde_json::from_str(&raw[raw.find("\r\n\r\n").unwrap() + 4..]).unwrap()
    }

    fn tool_message(name: &str, input: Value) -> Value {
        json!({
            "id": "msg", "type": "message", "role": "assistant", "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": "toolu", "name": name, "input": input}],
            "usage": {"input_tokens": 3, "output_tokens": 4}
        })
    }

    #[test]
    fn batch_body_wraps_each_request_body_under_its_custom_id() {
        let mut cfg = make_cfg(None);
        cfg.api_protocol = ApiProtocol::AnthropicMessages;
        let tool = json!({"type": "function", "name": "report"});
        let reqs: Vec<ResponseRequest> = ["a", "b"]
            .iter()
            .map(|input| ResponseRequest {
                input: Value::String((*input).into()),
                tools: vec![tool.clone()],
                tool_choice: Some("required".into()),
                system: Some("RULES".into()),
            })
            .collect();
        let body = build_anthropic_batch_body(&cfg, &reqs).unwrap();
        let requests = body["requests"].as_array().unwrap();
        assert_eq!(requests.len(), 2);
        for (i, (item, req)) in requests.iter().zip(&reqs).enumerate() {
            assert_eq!(item["custom_id"], format!("req-{i}"));
            // Same body as a direct call — cache breakpoints included.
            assert_eq!(
                item["params"],
                build_anthropic_request_body(&cfg, req).unwrap()
            );
        }
        assert_eq!(
            requests[1]["params"]["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn batch_results_follow_request_order_and_name_every_failure() {
        let jsonl = [
            json!({"custom_id": "req-2", "result": {"type": "expired"}}),
            json!({"custom_id": "req-0", "result": {"type": "succeeded", "message": {"id": "m0"}}}),
            json!({"custom_id": "req-1", "result": {"type": "errored", "error": {"type": "error", "error": {"type": "overloaded_error"}}}}),
        ]
        .map(|v| v.to_string())
        .join("\n");
        let results = parse_anthropic_batch_results(&jsonl, 4).unwrap();
        assert_eq!(results.len(), 4);
        assert_eq!(results[0].as_ref().unwrap()["id"], "m0");
        assert!(
            matches!(&results[1], Err(AiError::Batch(m)) if m.contains("overloaded_error")),
            "{:?}",
            results[1]
        );
        assert!(matches!(&results[2], Err(AiError::Batch(m)) if m.contains("expired")));
        assert!(matches!(&results[3], Err(AiError::Batch(m)) if m.contains("no result")));

        let unknown = json!({"custom_id": "req-9", "result": {"type": "expired"}}).to_string();
        assert!(matches!(
            parse_anthropic_batch_results(&unknown, 4),
            Err(AiError::Batch(_))
        ));
    }

    #[tokio::test]
    async fn respond_batch_creates_polls_and_reads_results_over_http() {
        let (addr, server) = scripted_anthropic_stub(|addr| {
            let results = [
                json!({"custom_id": "req-1", "result": {"type": "succeeded", "message": tool_message("report", json!({"n": 1}))}}),
                json!({"custom_id": "req-0", "result": {"type": "succeeded", "message": tool_message("report", json!({"n": 0}))}}),
            ]
            .map(|v| v.to_string())
            .join("\n");
            vec![
                (200, json!({"id": "msgbatch_1", "processing_status": "in_progress"}).to_string()),
                (200, json!({"id": "msgbatch_1", "processing_status": "in_progress"}).to_string()),
                (
                    200,
                    json!({"id": "msgbatch_1", "processing_status": "ended",
                           "results_url": format!("http://{addr}/v1/messages/batches/msgbatch_1/results")})
                    .to_string(),
                ),
                (200, results),
            ]
        });
        let reqs: Vec<ResponseRequest> = ["first", "second"]
            .iter()
            .map(|input| {
                make_req(
                    input,
                    vec![json!({"type": "function", "name": "report"})],
                    Some("required"),
                )
            })
            .collect();
        let wait = BatchWait {
            poll_interval: Duration::from_millis(5),
            deadline: Duration::from_secs(30),
        };
        let results = anthropic_stub_client(addr)
            .respond_batch(&reqs, wait)
            .await
            .unwrap();
        let args: Vec<String> = results
            .into_iter()
            .map(|outputs| match outputs.unwrap().pop() {
                Some(ResponseOutput::ToolCall(tc)) => tc.arguments,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(args, [r#"{"n":0}"#, r#"{"n":1}"#]);

        let requests = server.join().unwrap();
        assert_eq!(
            requests.iter().map(|r| request_line(r)).collect::<Vec<_>>(),
            [
                "POST /v1/messages/batches HTTP/1.1",
                "GET /v1/messages/batches/msgbatch_1 HTTP/1.1",
                "GET /v1/messages/batches/msgbatch_1 HTTP/1.1",
                "GET /v1/messages/batches/msgbatch_1/results HTTP/1.1",
            ]
        );
        for raw in &requests {
            let lower = raw.to_ascii_lowercase();
            assert!(lower.contains("x-api-key: sk-test"), "{raw}");
            assert!(lower.contains("anthropic-version: 2023-06-01"), "{raw}");
        }
        let created = request_body(&requests[0]);
        assert_eq!(created["requests"][0]["custom_id"], "req-0");
        assert_eq!(
            created["requests"][1]["params"]["messages"],
            json!([{"role": "user", "content": "second"}])
        );
    }

    #[tokio::test]
    async fn respond_batch_cancels_a_batch_past_its_deadline() {
        let (addr, server) = scripted_anthropic_stub(|_| {
            vec![
                (
                    200,
                    json!({"id": "msgbatch_2", "processing_status": "in_progress"}).to_string(),
                ),
                (
                    200,
                    json!({"id": "msgbatch_2", "processing_status": "canceling"}).to_string(),
                ),
            ]
        });
        let reqs = vec![make_req("x", vec![], None), make_req("y", vec![], None)];
        let wait = BatchWait {
            poll_interval: Duration::from_millis(5),
            deadline: Duration::ZERO,
        };
        let error = anthropic_stub_client(addr)
            .respond_batch(&reqs, wait)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, AiError::Batch(m) if m.contains("msgbatch_2") && m.contains("cancelled")),
            "{error:?}"
        );
        let requests = server.join().unwrap();
        assert_eq!(
            request_line(&requests[1]),
            "POST /v1/messages/batches/msgbatch_2/cancel HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn respond_batch_refuses_protocols_without_batches_before_any_request() {
        let client = OpenAiClient::new(make_cfg(None));
        let error = client
            .respond_batch(&[make_req("x", vec![], None)], BatchWait::default())
            .await
            .unwrap_err();
        assert!(matches!(error, AiError::Config(_)), "{error:?}");
    }

    #[tokio::test]
    async fn objects_batch_falls_back_to_one_call_per_prompt_when_the_batch_fails() {
        use crate::AiProvider;

        let (addr, server) = scripted_anthropic_stub(|_| {
            vec![
                (
                    500,
                    json!({"type": "error", "error": {"type": "api_error"}}).to_string(),
                ),
                (200, tool_message("report", json!({"n": "a"})).to_string()),
                (200, tool_message("report", json!({"n": "b"})).to_string()),
            ]
        });
        let tool = json!({"type": "function", "name": "report"});
        let results = anthropic_stub_client(addr)
            .generate_objects_batch(
                "RULES".into(),
                vec!["a".into(), "b".into()],
                vec![tool],
                "report",
            )
            .await;
        let args: Vec<Value> = results.into_iter().map(Result::unwrap).collect();
        assert_eq!(args, [json!({"n": "a"}), json!({"n": "b"})]);

        let requests = server.join().unwrap();
        assert_eq!(
            request_line(&requests[0]),
            "POST /v1/messages/batches HTTP/1.1"
        );
        for (raw, prompt) in requests[1..].iter().zip(["a", "b"]) {
            assert_eq!(request_line(raw), "POST /v1/messages HTTP/1.1");
            let sent = request_body(raw);
            assert_eq!(sent["messages"][0]["content"], prompt);
            assert_eq!(sent["system"][0]["text"], "RULES");
        }
    }
}
