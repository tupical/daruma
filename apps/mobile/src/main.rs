//! `daruma-mobile` — mobile client scaffold (§3.4 W3.1).
//!
//! Phase 1 is a minimal HTTP probe: `GET /v1/tasks` and print JSON to
//! stdout. A Tauri 2 shell will wrap this client once the mobile UI lands.
//!
//! Environment:
//!   * `DARUMA_API_URL` — server base (default `http://localhost:8080`)
//!   * `DARUMA_TOKEN`   — bearer token (required when auth is enabled)

use anyhow::Context;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let base =
        std::env::var("DARUMA_API_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let token = std::env::var("DARUMA_TOKEN").unwrap_or_default();
    let url = format!("{}/v1/tasks?status=active", base.trim_end_matches('/'));

    let http = reqwest::Client::builder()
        .user_agent(format!("daruma-mobile/{}", env!("CARGO_PKG_VERSION")))
        .build()?;

    let mut req = http.get(&url);
    if !token.trim().is_empty() {
        req = req.bearer_auth(token.trim());
    }

    let resp = req.send().await.context("GET /v1/tasks failed")?;
    let status = resp.status();
    let body = resp.text().await.context("read response body")?;
    if !status.is_success() {
        anyhow::bail!("GET /v1/tasks returned {status}: {body}");
    }

    let value: serde_json::Value =
        serde_json::from_str(&body).context("response is not valid JSON")?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
