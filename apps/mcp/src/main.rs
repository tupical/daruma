//! `taskagent-mcp` — stdio MCP server binary.
//!
//! Reads JSON-RPC frames from stdin, forwards each tool call to a
//! running `taskagent-server` over HTTP, and writes responses back to
//! stdout. Configuration is via environment:
//!
//!   * `TASKAGENT_API_URL` — base URL of the server (default
//!     `http://localhost:8080`).
//!   * `TASKAGENT_TOKEN`   — bearer token (required for any non-`healthz`
//!     tool call).
//!   * `TASKAGENT_WORKSPACE_ID` — optional logical workspace UUID.
//!   * `TASKAGENT_WORKSPACE` / `TASKAGENT_PROJECT_ID` — local workspace key
//!     and default project scope (see `docs/guides/mcp-client.md`).

use taskagent_mcp::{run_stdio, workspace::Workspace, ApiClient};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs go to stderr — stdout is reserved for the JSON-RPC channel.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let mut base =
        std::env::var("TASKAGENT_API_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let mut token = std::env::var("TASKAGENT_TOKEN").unwrap_or_default();
    let mut workspace_id_from_creds: Option<String> = None;

    if token.trim().is_empty() {
        if let Some(auth) = taskagent_mcp::credentials::resolve_from_agent_dir() {
            base = auth.api_url;
            token = auth.token;
            workspace_id_from_creds = auth.workspace_id;
            tracing::info!(
                path = %taskagent_mcp::credentials::credentials_path().display(),
                "loaded API credentials from agent dir"
            );
        }
    }

    if token.is_empty() {
        tracing::warn!(
            "TASKAGENT_TOKEN is empty — only /healthz will work; \
             set TASKAGENT_API_URL + TASKAGENT_TOKEN or save credentials.json"
        );
    }

    let ws = Workspace::init();
    tracing::info!(
        workspace = ws.key(),
        default_project = ?ws.default_project(),
        "workspace state loaded"
    );
    taskagent_mcp::workspace::install(ws);

    let http = reqwest::Client::builder()
        .user_agent(format!("taskagent-mcp/{}", env!("CARGO_PKG_VERSION")))
        .build()?;

    let mut client = ApiClient::with_http(base, token, http);
    let workspace_id = workspace_id_from_env().or(workspace_id_from_creds);
    if let Some(workspace_id) = workspace_id {
        tracing::info!(workspace_id = %workspace_id, "workspace scope configured");
        client = client.with_workspace_id(workspace_id);
    }

    tracing::info!("taskagent-mcp ready on stdio");
    run_stdio(client).await
}

/// Optional workspace scope sent as `X-TaskAgent-Workspace-Id`.
fn workspace_id_from_env() -> Option<String> {
    if let Ok(id) = std::env::var("TASKAGENT_WORKSPACE_ID") {
        let id = id.trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    if let Ok(key) = std::env::var("TASKAGENT_WORKSPACE") {
        let key = key.trim();
        if uuid::Uuid::parse_str(key).is_ok() {
            return Some(key.to_string());
        }
    }
    None
}

pub fn taskagent_mcp_bin() {}
