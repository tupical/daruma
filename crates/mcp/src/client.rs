//! Thin wrapper around `reqwest::Client` that knows how to talk to the
//! `daruma-server` HTTP surface — every tool handler funnels its HTTP
//! work through this client so the auth bearer is set in exactly one
//! place.

use daruma_shared::AgentId;
use serde_json::{json, Value};

/// HTTP-hop client used by every tool handler.
#[derive(Clone)]
pub struct ApiClient {
    base: String,
    token: String,
    /// Logical workspace scope sent as `X-Daruma-Workspace-Id`.
    workspace_id: Option<String>,
    http: reqwest::Client,
    /// Stable per-session agent id so every command this MCP process dispatches
    /// can be grouped under a single `Actor::Agent`. Generated once when the
    /// client is built; the server stores it on every emitted event.
    agent_id: String,
}

impl ApiClient {
    /// `base` is the server's root (no trailing slash), e.g.
    /// `http://localhost:8080`. `token` is the bearer.
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            token: token.into(),
            workspace_id: None,
            http: reqwest::Client::new(),
            agent_id: fresh_agent_id(),
        }
    }

    /// Attach a logical workspace UUID.
    pub fn with_workspace_id(mut self, workspace_id: impl Into<String>) -> Self {
        let id = workspace_id.into();
        if !id.trim().is_empty() {
            self.workspace_id = Some(id);
        }
        self
    }

    /// Build from a pre-existing `reqwest::Client` (lets the binary
    /// configure a user-agent / pool once).
    pub fn with_http(
        base: impl Into<String>,
        token: impl Into<String>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            base: base.into(),
            token: token.into(),
            workspace_id: None,
            http,
            agent_id: fresh_agent_id(),
        }
    }

    /// Stable MCP process agent id (UUID string).
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Wire-format Actor for every command this process dispatches.
    /// Matches `Actor::Agent` in `daruma-domain` (serde tag = "kind").
    fn actor_json(&self) -> Value {
        json!({ "kind": "agent", "id": self.agent_id, "name": "mcp" })
    }

    /// POST a command to `/v1/commands` with the MCP agent as the actor,
    /// so emitted events carry `Actor::Agent { name: "mcp" }` instead of
    /// falling back to the serde-default `Actor::User`.
    pub async fn post_command(&self, command: Value) -> anyhow::Result<Value> {
        let body = json!({ "command": command, "actor": self.actor_json() });
        let resp = self.post_json("/v1/commands", body).await?;
        Ok(enrich_mutation_response(resp))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base.trim_end_matches('/'), path)
    }

    fn auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let builder = builder
            .bearer_auth(&self.token)
            .header("X-Daruma-Plugin-Contract", "1");
        match &self.workspace_id {
            Some(id) => builder.header("X-Daruma-Workspace-Id", id.as_str()),
            None => builder,
        }
    }

    pub async fn get_json(&self, path: &str) -> anyhow::Result<Value> {
        let resp = self.auth(self.http.get(self.url(path))).send().await?;
        as_json(resp).await
    }

    pub async fn post_json(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        let resp = self
            .auth(self.http.post(self.url(path)))
            .json(&body)
            .send()
            .await?;
        Ok(enrich_mutation_response(as_json(resp).await?))
    }

    pub async fn put_json(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        let resp = self
            .auth(self.http.put(self.url(path)))
            .json(&body)
            .send()
            .await?;
        as_json(resp).await
    }

    pub async fn patch_json(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        let resp = self
            .auth(self.http.patch(self.url(path)))
            .json(&body)
            .send()
            .await?;
        Ok(enrich_mutation_response(as_json(resp).await?))
    }

    pub async fn delete_json(&self, path: &str) -> anyhow::Result<Value> {
        let resp = self.auth(self.http.delete(self.url(path))).send().await?;
        Ok(enrich_mutation_response(as_json(resp).await?))
    }
}

/// Render an `AgentId` as the bare UUID string — that's the wire format
/// `Actor::Agent { id }` uses on the server (AgentId is `#[serde(transparent)]`).
fn fresh_agent_id() -> String {
    AgentId::new().as_uuid().to_string()
}

async fn as_json(resp: reqwest::Response) -> anyhow::Result<Value> {
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("HTTP {status}: {text}");
    }
    if text.is_empty() {
        return Ok(Value::Null);
    }
    Ok(serde_json::from_str(&text)?)
}

fn enrich_mutation_response(mut value: Value) -> Value {
    let Some(obj) = value.as_object_mut() else {
        return value;
    };
    if obj.get("success").and_then(Value::as_bool) != Some(true) {
        return value;
    }

    let ids = collect_entity_ids(obj.get("data"));
    if ids.is_empty() {
        return value;
    }

    if matches!(obj.get("data"), Some(Value::Null)) {
        obj.insert("data".to_string(), Value::Object(Default::default()));
    }
    if let Some(data) = obj.get_mut("data").and_then(Value::as_object_mut) {
        for (key, id) in &ids {
            data.entry((*key).to_string()).or_insert_with(|| json!(id));
        }
    }
    for (key, id) in ids {
        obj.entry(key.to_string()).or_insert_with(|| json!(id));
    }
    value
}

fn collect_entity_ids(data: Option<&Value>) -> Vec<(&'static str, String)> {
    let mut ids = Vec::new();
    let Some(data) = data else {
        return ids;
    };

    collect_ids_from_value(data, &mut ids);
    if let Some(events) = data.as_array() {
        for event in events {
            if let Some(payload) = event.get("payload") {
                collect_ids_from_event_payload(payload, &mut ids);
            }
        }
    }
    dedupe_ids(ids)
}

fn collect_ids_from_value(value: &Value, ids: &mut Vec<(&'static str, String)>) {
    for (key, pointer) in [
        ("task_id", "/task/id"),
        ("project_id", "/project/id"),
        ("plan_id", "/plan/id"),
        ("run_id", "/run/id"),
        ("session_id", "/session/id"),
        ("document_id", "/document/id"),
        ("comment_id", "/comment/id"),
        ("relation_id", "/relation/id"),
    ] {
        push_id(ids, key, value.pointer(pointer).and_then(Value::as_str));
    }

    for key in [
        "task_id",
        "project_id",
        "plan_id",
        "run_id",
        "session_id",
        "document_id",
        "comment_id",
        "relation_id",
    ] {
        push_id(ids, key, value.get(key).and_then(Value::as_str));
    }
}

fn collect_ids_from_event_payload(payload: &Value, ids: &mut Vec<(&'static str, String)>) {
    match payload.get("type").and_then(Value::as_str) {
        Some("task_created") => push_id(
            ids,
            "task_id",
            payload.pointer("/task/id").and_then(Value::as_str),
        ),
        Some("project_created") => push_id(
            ids,
            "project_id",
            payload.pointer("/project/id").and_then(Value::as_str),
        ),
        Some("plan_created") => push_id(
            ids,
            "plan_id",
            payload.pointer("/plan/id").and_then(Value::as_str),
        ),
        Some("run_started") => push_id(
            ids,
            "run_id",
            payload.pointer("/run/id").and_then(Value::as_str),
        ),
        Some("agent_session_started") => push_id(
            ids,
            "session_id",
            payload.pointer("/session/id").and_then(Value::as_str),
        ),
        Some("document_created") => push_id(
            ids,
            "document_id",
            payload.pointer("/document/id").and_then(Value::as_str),
        ),
        Some("comment_added") => push_id(
            ids,
            "comment_id",
            payload.pointer("/comment/id").and_then(Value::as_str),
        ),
        Some("task_linked") => push_id(
            ids,
            "relation_id",
            payload.get("relation_id").and_then(Value::as_str),
        ),
        _ => {}
    }
}

fn push_id(ids: &mut Vec<(&'static str, String)>, key: &'static str, value: Option<&str>) {
    if let Some(value) = value {
        if !value.is_empty() {
            ids.push((key, value.to_string()));
        }
    }
}

fn dedupe_ids(ids: Vec<(&'static str, String)>) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for (key, value) in ids {
        if !out
            .iter()
            .any(|(existing_key, existing_value)| *existing_key == key && existing_value == &value)
        {
            out.push((key, value));
        }
    }
    out
}
