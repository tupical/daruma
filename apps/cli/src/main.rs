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
use std::path::{Path, PathBuf};
use taskagent_mcp::{run_stdio_with_profile, workspace::Workspace, ApiClient, ToolProfile};
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
    cmd: Option<Cmd>,
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
        /// Write the TaskAgent CLAUDE.md policy block (+ .omc OMC guard).
        #[arg(long)]
        claude: bool,
        /// Upsert taskagent MCP entry into Cursor mcp.json.
        #[arg(long)]
        cursor: bool,
        /// Upsert taskagent MCP entry into Windsurf mcp_config.json.
        #[arg(long)]
        windsurf: bool,
        /// Write the codex AGENTS.md managed policy block.
        #[arg(long)]
        codex: bool,
        /// Install all targets: cursor + windsurf + codex + claude.
        #[arg(long)]
        all: bool,
        /// Overwrite an existing taskagent entry instead of skipping.
        #[arg(long)]
        force: bool,
        /// Project directory for --claude / --codex / --cursor / --windsurf
        /// (default: current dir for policy targets; home dir for MCP configs).
        #[arg(long, value_name = "DIR")]
        project: Option<PathBuf>,
    },
    /// Run the stdio MCP server (JSON-RPC over stdin/stdout).
    ///
    /// This is the merged entry-point for what used to be the separate
    /// `taskagent-mcp` binary — one artifact configures and serves everything.
    /// Register with: `claude mcp add taskagent -- taskagent mcp`.
    Mcp {
        /// Tool surface profile: `default` (compact workflow set) or `full`
        /// (complete catalogue). Overrides TASKAGENT_MCP_PROFILE.
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
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
    let cli = Cli::parse();

    // Logs to stderr; stdout is reserved for the CLI's primary output (and for
    // the MCP JSON-RPC channel) so agents-through-shell can pipe cleanly. The
    // MCP server is chattier (info); the terse CLI stays quiet (warn) unless
    // RUST_LOG overrides.
    let default_level = if matches!(cli.cmd, Some(Cmd::Mcp { .. })) {
        "info"
    } else {
        "warn"
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

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

    match &cli.cmd {
        // Bare `taskagent` (no subcommand) → cloud-agnostic HTTP-MCP connect
        // guide. The launcher never reaches out anywhere; it only reflects
        // local credentials or prints the self-host quick start.
        None => return cmd_connect_guide(),
        Some(Cmd::Install {
            print_config,
            mode,
            yes,
            claude,
            cursor,
            windsurf,
            codex,
            all,
            force,
            project,
        }) => {
            return cmd_install(
                &base,
                &token,
                print_config.as_deref(),
                mode.as_deref(),
                *yes,
                *claude,
                *cursor,
                *windsurf,
                *codex,
                *all,
                *force,
                project.as_deref(),
            );
        }
        // Stdio MCP server: does its own env/credentials resolution and never
        // touches the table-rendering HTTP client below.
        Some(Cmd::Mcp { profile }) => return run_mcp_stdio(profile.as_deref()).await,
        _ => {}
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

    match cli.cmd.expect("None subcommand handled above") {
        Cmd::Install { .. } => unreachable!("install exits before HTTP client setup"), // all fields covered by `..`
        Cmd::Mcp { .. } => unreachable!("mcp exits before HTTP client setup"),
        Cmd::Next { project_id } => cmd_next(&client, project_id, cli.json).await,
        Cmd::Show { id } => cmd_show(&client, &id, cli.json).await,
        Cmd::Done { id } => cmd_done(&client, &id, cli.json).await,
        Cmd::List { project_id, status } => cmd_list(&client, project_id, &status, cli.json).await,
        Cmd::History {
            entity_type,
            entity_id,
            limit,
        } => cmd_history(&client, &entity_type, &entity_id, limit, cli.json).await,
    }
}

// ── stdio MCP server (merged `taskagent mcp`) ────────────────────────────────

/// Run the stdio MCP server: read JSON-RPC frames on stdin, forward each tool
/// call to a `taskagent-server` over HTTP, write responses to stdout. This is
/// the merged `taskagent mcp` entry-point (formerly the standalone
/// `taskagent-mcp` binary). Config is via env / credentials.json, exactly like
/// the rest of the CLI — fully generic over the server it points at.
async fn run_mcp_stdio(profile_flag: Option<&str>) -> anyhow::Result<()> {
    let profile = match profile_flag {
        Some(raw) => ToolProfile::parse(raw).ok_or_else(|| {
            anyhow::anyhow!("unknown MCP profile `{raw}` — expected `default` or `full`")
        })?,
        None => ToolProfile::from_env(),
    };
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

    tracing::info!(profile = profile.as_str(), "taskagent mcp ready on stdio");
    run_stdio_with_profile(client, profile).await
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

// ── command handlers ─────────────────────────────────────────────────────────

fn cmd_install(
    base: &str,
    token: &str,
    print_config: Option<&str>,
    mode: Option<&str>,
    yes: bool,
    claude: bool,
    cursor: bool,
    windsurf: bool,
    codex: bool,
    all: bool,
    force: bool,
    project: Option<&Path>,
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

    let do_claude = claude || all;
    let do_cursor = cursor || all;
    let do_windsurf = windsurf || all;
    let do_codex = codex || all;

    if do_cursor {
        let config_path = match project {
            Some(p) => p.join(".cursor").join("mcp.json"),
            None => home_dir().join(".cursor").join("mcp.json"),
        };
        install_mcp_json(&config_path, base, token, force)?;
        println!("cursor mcp.json:  {}", config_path.display());
    }

    if do_windsurf {
        let config_path = match project {
            Some(p) => p.join(".windsurf").join("mcp_config.json"),
            None => home_dir()
                .join(".codeium")
                .join("windsurf")
                .join("mcp_config.json"),
        };
        install_mcp_json(&config_path, base, token, force)?;
        println!("windsurf mcp_config.json: {}", config_path.display());
    }

    if do_codex {
        let dir = project
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        install_codex_policy(&dir)?;
    }

    if do_claude {
        let dir = project
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        install_claude_policy(&dir)?;
    }

    Ok(())
}

/// Returns the current user's home directory.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Upsert `mcpServers.taskagent` into a JSON config file (Cursor / Windsurf).
/// Preserves all other entries. Writes atomically (tmp + rename).
/// Returns an error-like message (not Err) if already present and !force — the
/// caller prints it directly so the overall install continues.
fn install_mcp_json(path: &Path, base: &str, token: &str, force: bool) -> anyhow::Result<()> {
    // Read existing or start fresh.
    let existing_raw = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: serde_json::Value = if existing_raw.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&existing_raw)
            .with_context(|| format!("parse existing JSON at {}", path.display()))?
    };

    if !doc.get("mcpServers").is_some_and(|v| v.is_object()) {
        doc["mcpServers"] = serde_json::json!({});
    }

    if doc["mcpServers"].get("taskagent").is_some() && !force {
        println!(
            "  already present: {} (use --force to overwrite)",
            path.display()
        );
        return Ok(());
    }

    // Build the entry — same shape as cursor_mcp_config() helper.
    let entry = {
        let mut e = serde_json::json!({
            "type": "http",
            "url": format!("{}/v1/mcp", base.trim_end_matches('/')),
        });
        if !token.trim().is_empty() {
            e["headers"] = serde_json::json!({
                "Authorization": format!("Bearer {}", token.trim()),
            });
        }
        e
    };
    doc["mcpServers"]["taskagent"] = entry;

    // Atomic write: tmp file in same directory, then rename.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)? + "\n")?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Cross-device rename (rare on Linux, possible on Windows).
        std::fs::copy(&tmp, path)?;
        std::fs::remove_file(&tmp).unwrap_or(());
        let _ = e; // original error ignored after successful copy
    }
    Ok(())
}

/// Markers for the codex AGENTS.md managed block. Kept byte-for-byte
/// compatible with clients/codex-plugin/lib/policy.mjs so re-runs from
/// either tool update the same block.
const CODEX_POLICY_BEGIN: &str = "<!-- taskagent-codex:policy:begin -->";
const CODEX_POLICY_END: &str = "<!-- taskagent-codex:policy:end -->";

/// Body of the codex policy block. The include_str! may carry a trailing
/// newline; we strip it so write_managed_block produces the same byte sequence
/// as the JS buildBlock() helper (`BEGIN\n<body>\nEND\n`).
const CODEX_POLICY_BODY_RAW: &str = include_str!("policy_codex.md");

fn codex_policy_body() -> &'static str {
    CODEX_POLICY_BODY_RAW.trim_end_matches('\n')
}

/// Write the codex policy block into `<project>/AGENTS.md`.
fn install_codex_policy(project_dir: &Path) -> anyhow::Result<()> {
    let agents_md = project_dir.join("AGENTS.md");
    write_managed_block(
        &agents_md,
        CODEX_POLICY_BEGIN,
        CODEX_POLICY_END,
        codex_policy_body(),
    )?;
    println!("codex policy written: {}", agents_md.display());
    Ok(())
}

/// Begin/end markers for the TaskAgent-managed CLAUDE.md policy block. Kept
/// byte-for-byte compatible with the installer wrappers so a later
/// re-run (binary or curl) updates the same block instead of appending.
const CLAUDE_POLICY_BEGIN: &str = "<!-- taskagent-claude:policy:begin -->";
const CLAUDE_POLICY_END: &str = "<!-- taskagent-claude:policy:end -->";
const OMC_GUARD_BEGIN: &str = "<!-- taskagent-claude:begin -->";
const OMC_GUARD_END: &str = "<!-- taskagent-claude:end -->";

/// The single source of truth for the TaskAgent project policy / OMC-guard
/// text. Wrappers (install.sh, the `taskagent-claude` npm plugin) and the
/// connect page call `taskagent install --claude` rather than carrying their
/// own copies. The bodies live in sibling `.md` files (canonical text + markers
/// match the npm plugin byte-for-byte so blocks stay idempotent across tools).
const CLAUDE_POLICY_BODY: &str = include_str!("policy_claude.md");

const OMC_GUARD_BODY: &str = include_str!("policy_omc_guard.md");

/// Write the TaskAgent policy block into `<project>/CLAUDE.md` and, when an
/// `.omc` directory is present, the OMC guard into `<project>/.omc/AGENTS.md`.
fn install_claude_policy(project_dir: &Path) -> anyhow::Result<()> {
    let claude_md = project_dir.join("CLAUDE.md");
    write_managed_block(
        &claude_md,
        CLAUDE_POLICY_BEGIN,
        CLAUDE_POLICY_END,
        CLAUDE_POLICY_BODY,
    )?;
    println!("claude policy written: {}", claude_md.display());

    if project_dir.join(".omc").is_dir() {
        let agents_md = project_dir.join(".omc/AGENTS.md");
        write_managed_block(&agents_md, OMC_GUARD_BEGIN, OMC_GUARD_END, OMC_GUARD_BODY)?;
        println!("omc guard written:    {}", agents_md.display());
    }
    Ok(())
}

/// Idempotently upsert a `begin`/`end`-delimited block in `path`: replace the
/// body between existing markers, or append a fresh block (creating the file
/// and parent dirs as needed). Mirrors the awk writer in the shell installer.
fn write_managed_block(path: &Path, begin: &str, end: &str, body: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let block = format!("{begin}\n{body}\n{end}\n");
    let existing = std::fs::read_to_string(path).unwrap_or_default();

    let updated = match (existing.find(begin), existing.find(end)) {
        (Some(b), Some(e)) if e > b => {
            let end_idx = e + end.len();
            // Drop a single trailing newline after the old end marker so we do
            // not accumulate blank lines across re-runs.
            let mut tail = &existing[end_idx..];
            tail = tail.strip_prefix('\n').unwrap_or(tail);
            format!("{}{}{}", &existing[..b], block, tail)
        }
        _ => {
            if existing.is_empty() {
                block
            } else if existing.ends_with('\n') {
                format!("{existing}\n{block}")
            } else {
                format!("{existing}\n\n{block}")
            }
        }
    };

    std::fs::write(path, updated)?;
    Ok(())
}

/// Default action for bare `taskagent`: print cloud-agnostic HTTP-MCP connect
/// instructions. With credentials present we echo a ready-to-paste snippet for
/// whatever server they point at (self-host or any other — the launcher does
/// not distinguish); without them we show the self-host quick start. No remote
/// account or service is referenced.
fn cmd_connect_guide() -> anyhow::Result<()> {
    match taskagent_mcp::credentials::resolve_from_agent_dir() {
        Some(auth) => {
            let server = auth.api_url.trim_end_matches('/');
            println!("TaskAgent configured → {server}");
            println!();
            println!("Connect Claude Code (HTTP MCP):");
            println!(
                "  claude mcp add --transport http taskagent {server}/v1/mcp \\\n    --header \"Authorization: Bearer {}\"",
                auth.token.trim()
            );
            println!();
            println!("Cursor (~/.cursor/mcp.json):");
            print_json(&cursor_mcp_config(server, &auth.token));
            println!();
            println!("Verify:  taskagent next");
        }
        None => {
            println!("No TaskAgent credentials found.");
            println!();
            println!("Self-host quick start (HTTP MCP — no account needed):");
            println!("  1) Start a local server:    taskagent-server");
            println!("  2) Save local credentials:  taskagent install --mode local -y");
            println!("  3) Connect Claude Code:     claude mcp add --transport http taskagent http://localhost:8080/v1/mcp");
            println!("  4) Verify:                  taskagent next");
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

    // ── install_mcp_json tests ────────────────────────────────────────────────

    #[test]
    fn install_mcp_json_writes_entry_into_empty_file() {
        let dir = std::env::temp_dir()
            .join(format!("taskagent-mcp-test-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(".cursor").join("mcp.json");

        install_mcp_json(&path, "http://localhost:8080", "mytoken", false).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            doc["mcpServers"]["taskagent"]["url"],
            "http://localhost:8080/v1/mcp"
        );
        assert_eq!(
            doc["mcpServers"]["taskagent"]["headers"]["Authorization"],
            "Bearer mytoken"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_mcp_json_preserves_foreign_entries() {
        let dir = std::env::temp_dir()
            .join(format!("taskagent-mcp-test-foreign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.json");

        // Pre-populate with a foreign entry.
        std::fs::write(
            &path,
            r#"{"mcpServers":{"other":{"type":"http","url":"http://other"}}}"#,
        )
        .unwrap();

        install_mcp_json(&path, "http://localhost:8080", "tok", false).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Foreign entry preserved.
        assert_eq!(doc["mcpServers"]["other"]["url"], "http://other");
        // New entry written.
        assert_eq!(
            doc["mcpServers"]["taskagent"]["url"],
            "http://localhost:8080/v1/mcp"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_mcp_json_skips_without_force_when_entry_exists() {
        let dir = std::env::temp_dir()
            .join(format!("taskagent-mcp-test-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.json");

        std::fs::write(
            &path,
            r#"{"mcpServers":{"taskagent":{"type":"http","url":"http://old/v1/mcp"}}}"#,
        )
        .unwrap();

        install_mcp_json(&path, "http://new", "newtoken", false).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Still the old URL — was skipped.
        assert_eq!(doc["mcpServers"]["taskagent"]["url"], "http://old/v1/mcp");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_mcp_json_force_overwrites_existing_entry() {
        let dir = std::env::temp_dir()
            .join(format!("taskagent-mcp-test-force-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.json");

        std::fs::write(
            &path,
            r#"{"mcpServers":{"taskagent":{"type":"http","url":"http://old/v1/mcp"}}}"#,
        )
        .unwrap();

        install_mcp_json(&path, "http://new", "newtoken", true).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(doc["mcpServers"]["taskagent"]["url"], "http://new/v1/mcp");
        assert_eq!(
            doc["mcpServers"]["taskagent"]["headers"]["Authorization"],
            "Bearer newtoken"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── install_codex_policy tests ────────────────────────────────────────────

    #[test]
    fn install_codex_policy_creates_agents_md() {
        let dir = std::env::temp_dir()
            .join(format!("taskagent-codex-test-create-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        install_codex_policy(&dir).unwrap();

        let body = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
        assert!(body.contains(CODEX_POLICY_BEGIN));
        assert!(body.contains(CODEX_POLICY_END));
        assert!(body.contains("taskagent_plan_create"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_codex_policy_idempotent_rerun() {
        let dir = std::env::temp_dir()
            .join(format!("taskagent-codex-test-idem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        install_codex_policy(&dir).unwrap();
        let first = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();

        install_codex_policy(&dir).unwrap();
        let second = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();

        assert_eq!(first, second, "second run must produce identical output");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_codex_policy_preserves_surrounding_content() {
        let dir = std::env::temp_dir()
            .join(format!("taskagent-codex-test-preserve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("AGENTS.md");
        std::fs::write(&path, "# My notes\nkeep this.\n").unwrap();

        install_codex_policy(&dir).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# My notes"), "original header preserved");
        assert!(body.contains("keep this."), "original content preserved");
        assert!(body.contains(CODEX_POLICY_BEGIN));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_policy_block_matches_js_marker_format() {
        // The block produced by write_managed_block must start/end with the
        // same markers the JS buildBlock() uses so cross-tool idempotency holds.
        let dir = std::env::temp_dir()
            .join(format!("taskagent-codex-test-markers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        install_codex_policy(&dir).unwrap();

        let body = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
        // Block must start with the begin marker and end with end marker + \n.
        assert!(
            body.contains(&format!("{CODEX_POLICY_BEGIN}\n")),
            "begin marker must be followed by newline"
        );
        assert!(
            body.contains(&format!("\n{CODEX_POLICY_END}\n")),
            "end marker must be preceded and followed by newline"
        );
        // No double-blank-line between body and end marker.
        assert!(
            !body.contains(&format!("\n\n{CODEX_POLICY_END}")),
            "no blank line between body and end marker"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
