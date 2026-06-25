//! Canonical agent-session metadata for IDE/MCP clients.
//!
//! Agents should call `daruma_session_start` with a `metadata` object so
//! tasks and comments can be traced back to a client chat / transcript.

use serde_json::{json, Map, Value};

/// Recommended keys written into `AgentSession.metadata`.
pub const KEY_CLIENT: &str = "client";
pub const KEY_MODEL: &str = "model";
pub const KEY_WORKSPACE_PATH: &str = "workspace_path";
pub const KEY_CHAT_ID: &str = "chat_id";
pub const KEY_TRANSCRIPT_PATH: &str = "transcript_path";
pub const KEY_HOST: &str = "host";

/// Merge caller `metadata` with env/workspace defaults (caller wins on conflict).
pub fn merge_defaults(mut metadata: Value) -> Value {
    let obj = metadata
        .as_object_mut()
        .map(|m| m.to_owned())
        .unwrap_or_default();
    let mut merged = Map::new();

    for (k, v) in default_entries() {
        merged.insert(k, v);
    }
    for (k, v) in obj {
        merged.insert(k, v);
    }

    Value::Object(merged)
}

fn default_entries() -> Vec<(String, Value)> {
    let mut out = Vec::new();

    if let Ok(client) = std::env::var("DARUMA_CLIENT") {
        if !client.trim().is_empty() {
            out.push((KEY_CLIENT.into(), json!(client.trim())));
        }
    }
    if let Ok(model) = std::env::var("DARUMA_MODEL") {
        if !model.trim().is_empty() {
            out.push((KEY_MODEL.into(), json!(model.trim())));
        }
    }
    if let Ok(chat_id) = std::env::var("DARUMA_CHAT_ID") {
        if !chat_id.trim().is_empty() {
            out.push((KEY_CHAT_ID.into(), json!(chat_id.trim())));
        }
    }
    if let Ok(path) = std::env::var("DARUMA_TRANSCRIPT_PATH") {
        if !path.trim().is_empty() {
            out.push((KEY_TRANSCRIPT_PATH.into(), json!(path.trim())));
        }
    }
    if let Ok(ws) = std::env::var("DARUMA_WORKSPACE") {
        if !ws.trim().is_empty() {
            out.push((KEY_WORKSPACE_PATH.into(), json!(ws.trim())));
        }
    } else if let Ok(cwd) = std::env::current_dir() {
        out.push((
            KEY_WORKSPACE_PATH.into(),
            json!(cwd.to_string_lossy().to_string()),
        ));
    }
    if let Ok(host) = std::env::var("DARUMA_HOST") {
        if !host.trim().is_empty() {
            out.push((KEY_HOST.into(), json!(host.trim())));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env_lock;

    #[test]
    fn merge_defaults_preserves_caller_overrides() {
        let _guard = env_lock();
        std::env::set_var("DARUMA_CLIENT", "cursor");
        std::env::set_var("DARUMA_MODEL", "env-model");

        let merged = merge_defaults(json!({
            "client": "codex",
            "model": "caller-model",
            "chat_id": "chat-1"
        }));

        assert_eq!(merged["client"], "codex");
        assert_eq!(merged["model"], "caller-model");
        assert_eq!(merged["chat_id"], "chat-1");
        assert!(merged.get(KEY_WORKSPACE_PATH).is_some());

        std::env::remove_var("DARUMA_CLIENT");
        std::env::remove_var("DARUMA_MODEL");
    }
}
