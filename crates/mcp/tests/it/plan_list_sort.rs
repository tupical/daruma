//! Task 01a144c6: `/plan` and `/next` pick "the most recently updated active
//! plan", but `/v1/plans` lists oldest-created first and the MCP tool pages
//! by 10 — the latest-updated plan can sit past the first page.
//! `daruma_plan_list sort=updated` orders before paging, and every command
//! template (local files and the server prompt catalogue) asks for it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, routing::any, Router};
use daruma_mcp::prompts::prompt_body;
use daruma_mcp::tools::call_tool;
use daruma_mcp::ApiClient;
use serde_json::{json, Value};
use tokio::net::TcpListener;

/// Twelve active plans in creation order. The latest-updated one is the
/// twelfth — past the default first page of ten. Fraction widths vary the
/// way chrono writes them (`…:07Z`, `…:07.5Z`).
fn plans() -> Value {
    let mut rows: Vec<Value> = (1..=12)
        .map(|n| {
            json!({
                "id": format!("pln_{n:02}"),
                "title": format!("plan {n}"),
                "status": "active",
                "project_id": "prj_1",
                "updated_at": format!("2026-10-{n:02}T10:00:00Z"),
            })
        })
        .collect();
    rows[11]["updated_at"] = json!("2026-10-20T10:00:07Z");
    // Same second as plan 12 but later by half a second: must win.
    rows[2]["updated_at"] = json!("2026-10-20T10:00:07.5Z");
    Value::Array(rows)
}

async fn serve() -> (
    ApiClient,
    Arc<Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let seen = captured.clone();
    let router = Router::new().fallback(any(move |req: Request<Body>| {
        let seen = seen.clone();
        async move {
            let path = req
                .uri()
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_default();
            seen.lock().unwrap().push(path.clone());
            assert!(path.starts_with("/v1/plans?"), "unexpected request: {path}");
            axum::Json(plans())
        }
    }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (ApiClient::new(base, "test-token"), captured, handle)
}

fn ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap_or_else(|| panic!("paged rows: {page}"))
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn sort_updated_returns_latest_updated_plan_first() {
    let (client, captured, handle) = serve().await;
    let latest = call_tool(
        &client,
        "daruma_plan_list",
        json!({"project_id": "prj_1", "status": "active", "sort": "updated", "limit": 1}),
    )
    .await
    .unwrap();
    let default_page = call_tool(
        &client,
        "daruma_plan_list",
        json!({"project_id": "prj_1", "status": "active"}),
    )
    .await
    .unwrap();
    let first = call_tool(
        &client,
        "daruma_plan_list",
        json!({"project_id": "prj_1", "status": "active", "sort": "updated", "limit": 3}),
    )
    .await
    .unwrap();
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    let second = call_tool(
        &client,
        "daruma_plan_list",
        json!({"project_id": "prj_1", "status": "active", "sort": "updated", "limit": 3, "cursor": cursor}),
    )
    .await
    .unwrap();
    handle.abort();

    assert_eq!(ids(&latest), ["pln_03"]);
    assert_eq!(latest["items"][0]["updated_at"], "2026-10-20T10:00:07.5Z");
    // Default order is unchanged: creation order, and the latest-updated
    // plan 12 is not on the first page — the bug the sort fixes.
    let default_ids = ids(&default_page);
    assert_eq!(default_ids.len(), 10);
    assert_eq!(default_ids[0], "pln_01");
    assert!(!default_ids.contains(&"pln_12".to_owned()));
    // The cursor continues the updated order.
    assert_eq!(ids(&first), ["pln_03", "pln_12", "pln_11"]);
    assert_eq!(ids(&second), ["pln_10", "pln_09", "pln_08"]);
    // Sorting is local to the MCP tool: the REST query is the same.
    let paths = captured.lock().unwrap().clone();
    assert!(
        paths
            .iter()
            .all(|p| p == "/v1/plans?status=active&project_id=prj_1"),
        "{paths:?}"
    );
}

#[tokio::test]
async fn unknown_sort_is_rejected_before_any_request() {
    let (client, captured, handle) = serve().await;
    let err = call_tool(
        &client,
        "daruma_plan_list",
        json!({"project_id": "prj_1", "sort": "newest"}),
    )
    .await
    .unwrap_err()
    .to_string();
    handle.abort();
    assert_eq!(
        err,
        "unknown sort: \"newest\" (expected one of: created, updated)"
    );
    assert!(captured.lock().unwrap().is_empty());
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn without_frontmatter(markdown: &str) -> &str {
    markdown
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map_or(markdown, |(_, body)| body.trim_start_matches('\n'))
}

/// The server prompt catalogue is a copy of the Cursor commands (the Docker
/// build has no `clients/`, so it cannot `include_str!` them). Any drift
/// between the copy and its source fails here instead of reaching agents.
#[test]
fn server_prompts_match_cursor_command_files() {
    for name in ["daruma-tasks", "daruma-plan", "daruma-next", "daruma-mine"] {
        let path = repo_root().join(format!("clients/cursor-plugin/cursor/commands/{name}.md"));
        let source = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            prompt_body(name).unwrap(),
            without_frontmatter(&source),
            "crates/mcp/src/prompts.rs `{name}` drifted from {}; copy the file body",
            path.display()
        );
    }
}

/// Every command that picks "the most recently updated" active plan asks
/// the server for that order instead of comparing a first page.
#[test]
fn plan_picking_commands_ask_for_updated_order() {
    for file in [
        "clients/claude-plugin/commands/plan.md",
        "clients/claude-plugin/commands/next.md",
        "clients/claude-plugin/commands/status.md",
        "clients/codex-plugin/commands/plan.md",
        "clients/codex-plugin/commands/next.md",
        "clients/cursor-plugin/cursor/commands/daruma-plan.md",
        "clients/cursor-plugin/cursor/commands/daruma-next.md",
    ] {
        let text = std::fs::read_to_string(repo_root().join(file)).unwrap();
        let compact: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            compact.contains(r#"sort = "updated""#) || compact.contains(r#"sort="updated""#),
            "{file}: plan pick must pass sort=updated to daruma_plan_list"
        );
        assert!(
            !compact.contains("most recent.") && !compact.contains("pick most recent"),
            "{file}: still picks the most recent plan by eye"
        );
    }
}
