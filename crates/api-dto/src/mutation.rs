//! Standard mutation response envelope (§3.9 Linear A.1).
//!
//! Previously defined as `MutationResponse` in `apps/server/src/routes/mod.rs`.

use daruma_shared::EventId;
use serde::{Deserialize, Serialize};

/// Non-fatal mutation warning. Clients can surface these without treating the
/// mutation as failed.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct MutationWarning {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

/// Standard response shape for every mutation endpoint (§3.9 Linear A.1).
///
/// The generic parameter `D` defaults to `serde_json::Value` so the server
/// can attach arbitrary event payloads while the WASM client deserialises
/// with `D = serde_json::Value` without needing the full type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationResponse<D = serde_json::Value> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<EventId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_seq: Option<u64>,
    pub data: D,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<MutationWarning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_command_id: Option<uuid::Uuid>,
}
