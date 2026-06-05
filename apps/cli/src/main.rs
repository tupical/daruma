//! `taskagent` — terse CLI for humans-and-agents-via-shell over the
//! `taskagent-server` HTTP surface (§3.8.11, CTM B.5).
//!
//! Thin wrapper around `taskagent_mcp::ApiClient`. Output is either a
//! `comfy-table` for humans or raw JSON for scripts via `--json`.
//!
//! Verbs (terse on purpose):
//!   * `taskagent next`            — pick the next claim-ready task
//!   * `taskagent show <id>`       — task + comments
//!   * `taskagent done <id>`       — mark a task done
//!   * `taskagent list --status <filter>` — list with required status filter
//!   * `taskagent history task <id>` — version timeline
//!
//! Environment:
//!   * `TASKAGENT_API_URL`     — server base (default `http://localhost:8080`)
//!   * `TASKAGENT_TOKEN`       — bearer token
//!   * `TASKAGENT_PROJECT_ID`  — scope for `next` / `list` when no `--project-id`
//!   * `TASKAGENT_WORKSPACE`   — optional workspace key override

use anyhow::Context;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use taskagent_mcp::{workspace::Workspace, ApiClient};
use tracing_subscriber::EnvFilter;

mod format;

/// Top-level CLI entry-point.
#[derive(Debug, Parser)]
#[command(
    name = "taskagent",
    version,
    about = "Terse CLI for taskagent — next / show / done / list",
    long_about = None
)]
struct Cli {
    /// Emit JSON instead of a human-readable table.
    #[arg(long, global = true)]
    json: bool,

    /// Server base URL (default `http://localhost:8080`).
    #[arg(long, global = true, env = "TASKAGENT_API_URL")]
    api_url: Option<String>,

    /// Bearer token. May be empty for `/healthz`.
    #[arg(long, global = true, env = "TASKAGENT_TOKEN")]
    token: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Configure taskagent integrations.
    Install {
        /// Print an MCP config snippet instead of writing files.
        #[arg(long = "print-config", value_name = "TARGET")]
        print_config: Option<String>,
        /// Persist credentials for a connection mode.
        #[arg(long, value_name = "MODE")]
        mode: Option<String>,
        /// Do not prompt. Required when writing credentials.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Show the next claim-ready task in the current project.
    Next {
        /// Project id (defaults to env / workspace).
        #[arg(long, env = "TASKAGENT_PROJECT_ID")]
        project_id: Option<String>,
    },
    /// Show a task and its comments.
    Show {
        /// Task id.
        id: String,
    },
    /// Mark a task done.
    Done {
        /// Task id.
        id: String,
    },
    /// List tasks (requires an explicit status filter).
    List {
        /// Project id (defaults to env / workspace). Pass `all` to ignore the default.
        #[arg(long, env = "TASKAGENT_PROJECT_ID")]
        project_id: Option<String>,
        /// Required status filter (`active`, `all`, `todo`, `in_progress`, `done`, …).
        #[arg(long)]
        status: String,
    },
    /// Show version history for a task or document.
    History {
        /// Entity type (`task` or `document`).
        entity_type: String,
        /// Entity id (raw UUID or display-prefixed id).
        entity_id: String,
        /// Maximum versions to show.
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs to stderr; stdout is reserved for the CLI's primary output so
    // agents-through-shell can pipe `--json` cleanly.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let base = cli
        .api_url
        .clone()
        .or_else(|| taskagent_mcp::credentials::resolve_from_agent_dir().map(|c| c.api_url))
        .unwrap_or_else(|| "http://localhost:8080".to_string());
    let token = cli
        .token
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| taskagent_mcp::credentials::resolve_from_agent_dir().map(|c| c.token))
        .unwrap_or_default();

    if let Cmd::Install {
        print_config,
        mode,
        yes,
    } = &cli.cmd
    {
        return cmd_install(
            &base,
            &token,
            print_config.as_deref(),
            mode.as_deref(),
            *yes,
        );
    }

    // Install workspace state so the `taskagent-mcp` workspace helpers
    // (used here for `default_project`) work the same way they do in the
    // MCP binary — single source of truth for "current project".
    let ws = Workspace::init();
    taskagent_mcp::workspace::install(ws);

    let http = reqwest::Client::builder()
        .user_agent(format!("taskagent-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let client = ApiClient::with_http(base.clone(), token.clone(), http);

    match cli.cmd {
        Cmd::Install { .. } => unreachable!("install exits before HTTP client setup"),
        Cmd::Next { project_id } => cmd_next(&client, project_id, cli.json).await,
        Cmd::Show { id } => cmd_show(&client, &id, cli.json).await,
        Cmd::Done { id } => cmd_done(&client, &id, cli.json).await,
        Cmd::List {
            project_id,
            status,
        } => cmd_list(&client, project_id, &status, cli.json).await,
        Cmd::History {
            entity_type,
            entity_id,
            limit,
        } => cmd_history(&client, &entity_type, &entity_id, limit, cli.json).await,
    }
}

// ── command handlers ─────────────────────────────────────────────────────────

fn cmd_install(
    base: &str,
    token: &str,
    print_config: Option<&str>,
    mode: Option<&str>,
    yes: bool,
) -> anyhow::Result<()> {
    if let Some(mode) = mode {
        save_credentials(mode, base, token, yes)?;
    }

    if let Some(target) = print_config {
        match target {
            "cursor" => {
                print_json(&cursor_mcp_config(base, token));
                return Ok(());
            }
            other => anyhow::bail!("unsupported --print-config target: {other}"),
        }
    }
    Ok(())
}

fn cursor_mcp_config(base: &str, token: &str) -> Value {
    let mut entry = json!({
        "url": format!("{}/v1/mcp", base.trim_end_matches('/')),
    });
    if !token.trim().is_empty() {
        entry["headers"] = json!({
            "Authorization": format!("Bearer {}", token.trim()),
        });
    }
    json!({
        "mcpServers": {
            "taskagent": entry,
        }
    })
}

fn save_credentials(mode: &str, base: &str, token: &str, yes: bool) -> anyhow::Result<()> {
    if mode != "self-host" && mode != "local" {
        anyhow::bail!("unsupported install mode: {mode}");
    }
    if !yes {
        anyhow::bail!("writing credentials requires -y/--yes");
    }
    let token = if token.trim().is_empty() && mode == "local" {
        read_local_bootstrap_token()?
    } else {
        token.trim().to_string()
    };
    if token.is_empty() {
        anyhow::bail!("TASKAGENT_TOKEN or --token is required");
    }

    let path = taskagent_mcp::credentials::credentials_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut doc = read_credentials_doc(&path)?;
    let profile_name = format!("{mode}-default");
    doc["schema_version"] = json!(1);
    doc["active_profile"] = json!(profile_name);
    if !doc.get("profiles").is_some_and(Value::is_object) {
        doc["profiles"] = json!({});
    }
    doc["profiles"][profile_name] = json!({
        "mode": mode,
        "server_url": base.trim_end_matches('/'),
        "token": token,
    });
    write_credentials_doc(&path, &doc)?;
    println!("credentials saved: {}", path.display());
    Ok(())
}

fn read_local_bootstrap_token() -> anyhow::Result<String> {
    let data_dir = taskagent_mcp::paths::data_dir();
    let path = data_dir.join("bootstrap.token");
    let token = std::fs::read_to_string(&path)
        .with_context(|| format!("read local bootstrap token at {}", path.display()))?;
    Ok(token.trim().to_string())
}

fn read_credentials_doc(path: &std::path::Path) -> anyhow::Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(serde_json::from_str(&raw)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(json!({ "schema_version": 1, "profiles": BTreeMap::<String, Value>::new() }))
        }
        Err(err) => Err(err.into()),
    }
}

fn write_credentials_doc(path: &std::path::Path, doc: &Value) -> anyhow::Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(doc)? + "\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Resolve the project id to use: explicit `--project-id` > env > workspace
/// default. Returns `None` when nothing is configured (caller decides what
/// to do — `list` falls back to "everything").
fn resolve_project(cli_arg: Option<String>) -> Option<String> {
    if let Some(p) = cli_arg.filter(|p| !p.is_empty()) {
        return Some(p);
    }
    taskagent_mcp::workspace::global().and_then(|w| w.default_project())
}

async fn cmd_next(
    client: &ApiClient,
    project_id: Option<String>,
    as_json: bool,
) -> anyhow::Result<()> {
    let pid = resolve_project(project_id);
    let mut params = vec![("status", "active".to_string())];
    match pid.as_deref() {
        Some("all") => {}
        Some(p) => params.push(("project_id", urlencode(p))),
        None => {}
    }
    let qs = params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let tasks = client
        .get_json(&format!("/v1/tasks?{qs}"))
        .await
        .context("GET tasks failed")?;
    let arr = tasks.as_array().cloned().unwrap_or_default();
    let next = format::pick_next(&arr);
    match next {
        Some(t) => {
            if as_json {
                print_json(&t);
            } else {
                println!("{}", format::task_table(std::slice::from_ref(&t)));
            }
            Ok(())
        }
        None => {
            if as_json {
                print_json(&Value::Null);
            } else {
                println!("(no claim-ready tasks)");
            }
            Ok(())
        }
    }
}

async fn cmd_show(client: &ApiClient, id: &str, as_json: bool) -> anyhow::Result<()> {
    let task = client
        .get_json(&format!("/v1/tasks/{}", urlencode(id)))
        .await
        .context("GET task failed")?;
    // Comments are a separate endpoint; surface them inline.
    let comments = client
        .get_json(&format!("/v1/tasks/{}/comments", urlencode(id)))
        .await
        .unwrap_or(Value::Array(vec![]));
    if as_json {
        let combined = json!({ "task": task, "comments": comments });
        print_json(&combined);
        return Ok(());
    }
    println!("{}", format::task_detail(&task));
    let cs = comments.as_array().cloned().unwrap_or_default();
    if !cs.is_empty() {
        println!();
        println!("Comments:");
        println!("{}", format::comments_table(&cs));
    }
    Ok(())
}

async fn cmd_done(client: &ApiClient, id: &str, as_json: bool) -> anyhow::Result<()> {
    let resp = client
        .post_command(json!({ "type": "complete_task", "id": id }))
        .await
        .context("complete_task failed")?;
    if as_json {
        print_json(&resp);
    } else {
        println!("done: {id}");
    }
    Ok(())
}

async fn cmd_list(
    client: &ApiClient,
    project_id: Option<String>,
    status: &str,
    as_json: bool,
) -> anyhow::Result<()> {
    let pid = resolve_project(project_id);
    let mut params = vec![("status", urlencode(status.trim()))];
    match pid.as_deref() {
        Some("all") => {}
        Some(p) => params.push(("project_id", urlencode(p))),
        None => {}
    }
    let qs = params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let tasks = client
        .get_json(&format!("/v1/tasks?{qs}"))
        .await
        .context("GET tasks failed")?;
    let arr = tasks.as_array().cloned().unwrap_or_default();
    if as_json {
        print_json(&Value::Array(arr));
    } else if arr.is_empty() {
        println!("(no tasks)");
    } else {
        println!("{}", format::task_table(&arr));
    }
    Ok(())
}

async fn cmd_history(
    client: &ApiClient,
    entity_type: &str,
    entity_id: &str,
    limit: u32,
    as_json: bool,
) -> anyhow::Result<()> {
    let path = format!(
        "/v1/history?entity_type={}&entity_id={}&limit={limit}",
        urlencode(entity_type),
        urlencode(entity_id)
    );
    let history = client.get_json(&path).await.context("GET history failed")?;
    let arr = history.as_array().cloned().unwrap_or_default();
    if as_json {
        print_json(&Value::Array(arr));
    } else if arr.is_empty() {
        println!("(no history)");
    } else {
        println!("{}", format::history_table(&arr));
    }
    Ok(())
}

// ── utilities ────────────────────────────────────────────────────────────────

fn print_json(v: &Value) {
    // `to_string_pretty` is fine for the human eye and stays valid for jq.
    match serde_json::to_string_pretty(v) {
        Ok(s) => println!("{s}"),
        Err(_) => println!("null"),
    }
}

/// Same percent-encoder as `taskagent-mcp::tools::urlencode` — kept private
/// to avoid widening that crate's public API for one helper.
fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn cursor_config_uses_remote_mcp_url_and_bearer_header() {
        let config = cursor_mcp_config("http://localhost:8080/", "ta_svc_test");
        assert_eq!(
            config["mcpServers"]["taskagent"]["url"],
            "http://localhost:8080/v1/mcp"
        );
        assert_eq!(
            config["mcpServers"]["taskagent"]["headers"]["Authorization"],
            "Bearer ta_svc_test"
        );
    }

    #[test]
    fn cursor_config_omits_empty_token_header() {
        let config = cursor_mcp_config("http://localhost:8080", "");
        assert!(config["mcpServers"]["taskagent"].get("headers").is_none());
    }

    #[test]
    fn save_self_host_credentials_writes_active_profile() {
        let _guard = env_lock().lock().unwrap();
        let dir =
            std::env::temp_dir().join(format!("taskagent-cli-cred-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var(taskagent_mcp::paths::ENV_AGENT_DIR, &dir);

        save_credentials("self-host", "http://localhost:8080/", "ta_svc_test", true).unwrap();

        let path = taskagent_mcp::credentials::credentials_path();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(doc["active_profile"], "self-host-default");
        assert_eq!(
            doc["profiles"]["self-host-default"]["server_url"],
            "http://localhost:8080"
        );
        assert_eq!(doc["profiles"]["self-host-default"]["token"], "ta_svc_test");

        std::env::remove_var(taskagent_mcp::paths::ENV_AGENT_DIR);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_local_credentials_reads_bootstrap_token() {
        let _guard = env_lock().lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "taskagent-cli-local-cred-test-{}",
            std::process::id()
        ));
        let agent_dir = root.join("agent");
        let data_dir = root.join("data");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("bootstrap.token"), "ta_svc_bootstrap\n").unwrap();
        std::env::set_var(taskagent_mcp::paths::ENV_AGENT_DIR, &agent_dir);
        std::env::set_var("TASKAGENT_DATA_DIR", &data_dir);

        save_credentials("local", "http://localhost:8080", "", true).unwrap();

        let path = taskagent_mcp::credentials::credentials_path();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(doc["active_profile"], "local-default");
        assert_eq!(
            doc["profiles"]["local-default"]["token"],
            "ta_svc_bootstrap"
        );

        std::env::remove_var(taskagent_mcp::paths::ENV_AGENT_DIR);
        std::env::remove_var("TASKAGENT_DATA_DIR");
        let _ = std::fs::remove_dir_all(&root);
    }
}
