//! Minimal MCP server for Daruma.
//!
//! Implements just enough of the Model Context Protocol (JSON-RPC 2.0 over
//! stdio) for tool-call interop with Claude Desktop and the MCP Inspector:
//!
//!   * `initialize`     — capability handshake
//!   * `prompts/list`   — discover built-in workflow prompts
//!   * `prompts/get`    — fetch a built-in workflow prompt
//!   * `tools/list`     — discover available tools
//!   * `tools/call`     — invoke a tool by name with JSON arguments
//!
//! The implementation is deliberately dependency-light — no `rmcp` SDK,
//! just `serde_json` + `reqwest` — so the binary works against any
//! MCP-compatible client without pulling an external runtime.
//!
//! ## Transport
//! Each message is a single line of JSON (LSP-style newline framing).
//! Requests are dispatched via [`dispatch_request`]; the stdio loop in
//! [`server::run_stdio`] wires them to async stdin/stdout.

pub mod client;
pub mod credentials;
pub mod paths;
pub mod prompts;
pub mod protocol;
pub mod server;
pub mod session_metadata;
pub mod tools;
pub mod workspace;

pub use client::ApiClient;
pub use protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
pub use server::{
    dispatch_request, dispatch_request_with_profile, run_stdio, run_stdio_with_profile,
};
pub use tools::{
    tool_definitions, tool_definitions_for, tool_hidden_in_profile, ToolAnnotations,
    ToolDefinition, ToolDomain, ToolProfile,
};

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }
}
