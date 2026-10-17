//! End-to-end integration tests for WorkspaceGraph HTTP endpoints (P3).

use axum::http::StatusCode;
use daruma_auth::{Capabilities, Capability};
use daruma_events::EventStore;
use daruma_shared::{ProjectId, TaskId};
use serde_json::Value;

mod common;
use common::{json_get, json_post, mint_pat, test_app};
use daruma_server::workspace_graph;

async fn sync_graph(app: &common::TestApp) {
    workspace_graph::catch_up_from_events(&app.state.workspace_graph, &*app.event_store())
        .await
        .unwrap();
}

fn graph_task_node(raw: &str) -> String {
    format!("task:{}", raw.parse::<TaskId>().unwrap())
}

fn graph_project_node(raw: &str) -> String {
    format!("project:{}", raw.parse::<ProjectId>().unwrap())
}

async fn create_project(app: &common::TestApp, token: &str, title: &str) -> String {
    let (_, json) = json_post(
        app.router.clone(),
        token,
        "/v1/commands",
        &format!(r#"{{"command":{{"type":"create_project","title":"{title}"}}}}"#),
    )
    .await;
    json["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == "project_created" {
                p.get("project")?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("project_created event with project.id")
}

async fn create_task(app: &common::TestApp, token: &str, project_id: &str, title: &str) -> String {
    let (_, json) = json_post(
        app.router.clone(),
        token,
        "/v1/commands",
        &format!(
            r#"{{"command":{{"type":"create_task","task":{{"project_id":"{project_id}","title":"{title}"}}}}}}"#
        ),
    )
    .await;
    json["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == "task_created" {
                p.get("task")?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("task_created event with task.id")
}

#[tokio::test]
async fn status_reflects_task_after_create() {
    let app = test_app().await;
    let token = app.admin_token.clone();

    let project_id = create_project(&app, &token, "Graph demo").await;
    let task_id = create_task(&app, &token, &project_id, "Index me").await;
    sync_graph(&app).await;

    let (status, json) = json_get(app.router.clone(), &token, "/v1/workspacegraph/status").await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        json["node_count"].as_u64().unwrap() >= 2,
        "expected project + task nodes"
    );
    assert!(json["last_event_seq"].as_u64().is_some());

    let node_id = graph_task_node(&task_id);
    let (status, ctx) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/workspacegraph/context?node_id={node_id}&limit=10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctx.as_array().unwrap().iter().any(|item| {
        item["edge"]["kind"].as_str() == Some("Contains")
            && item["node"]["id"].as_str() == Some(&graph_project_node(&project_id))
    }));
}

#[tokio::test]
async fn context_and_search_after_linking_tasks() {
    let app = test_app().await;
    let token = app.admin_token.clone();

    let project_id = create_project(&app, &token, "Link demo").await;
    let from_id = create_task(&app, &token, &project_id, "Blocker apricot task").await;
    let to_id = create_task(&app, &token, &project_id, "Blocked follow-up").await;

    let (link_status, link_json) = json_post(
        app.router.clone(),
        &token,
        "/v1/relations",
        &format!(r#"{{"from":"{from_id}","to":"{to_id}","kind":"blocks"}}"#),
    )
    .await;
    assert_eq!(link_status, StatusCode::CREATED);
    assert_eq!(link_json["success"], Value::Bool(true));
    sync_graph(&app).await;

    let from_node = graph_task_node(&from_id);
    let (status, ctx) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/workspacegraph/context?node_id={from_node}&limit=20"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        ctx.as_array().unwrap().iter().any(|item| {
            item["edge"]["kind"].as_str() == Some("Blocks")
                && item["node"]["id"].as_str() == Some(&graph_task_node(&to_id))
        }),
        "linked task should appear in context: {ctx}"
    );

    let (status, hits) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/workspacegraph/search?query=apricot&limit=10&project_id={project_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        hits.as_array()
            .unwrap()
            .iter()
            .any(|hit| { hit["node"]["id"].as_str() == Some(&graph_task_node(&from_id)) }),
        "search should find the blocker task: {hits}"
    );
}

#[tokio::test]
async fn requires_task_read_capability() {
    let app = test_app().await;
    let (read_token, _) = mint_pat(
        &app.auth_store(),
        Capabilities::from([Capability::TaskRead]),
        daruma_auth::ProjectFilter::All,
    )
    .await;
    let (no_read_token, _) = mint_pat(
        &app.auth_store(),
        Capabilities::from([Capability::ProjectRead]),
        daruma_auth::ProjectFilter::All,
    )
    .await;

    let (status, _) = json_get(app.router.clone(), &read_token, "/v1/workspacegraph/status").await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = json_get(
        app.router.clone(),
        &no_read_token,
        "/v1/workspacegraph/status",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn resolves_node_id_from_kind_and_source_id() {
    let app = test_app().await;
    let token = app.admin_token.clone();

    let project_id = create_project(&app, &token, "Kind param demo").await;
    let task_id = create_task(&app, &token, &project_id, "Kind lookup").await;
    sync_graph(&app).await;

    let (status, ctx) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/workspacegraph/context?kind=task&source_id={task_id}&limit=5"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!ctx.as_array().unwrap().is_empty());
}

async fn graph_hits(app: &common::TestApp, token: &str, query: &str) -> Vec<String> {
    let (status, json) = json_get(
        app.router.clone(),
        token,
        &format!("/v1/workspacegraph/search?query={query}&limit=10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    json.as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["node"]["title"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// Corporate-game scenario: the operator backs up the workspace with the
/// documented `VACUUM INTO`, keeps working, then restores the backup without
/// touching `workspacegraph.sqlite`. The graph must answer for the restored
/// history, not for the work the restore rolled back.
#[tokio::test]
async fn restore_from_older_backup_rebuilds_stale_graph_scenario() {
    let app = test_app().await;
    let token = app.admin_token.clone();

    let project_id = create_project(&app, &token, "Corporate game").await;
    create_task(&app, &token, &project_id, "Quarterly budget review").await;
    sync_graph(&app).await;
    let backup_path = std::env::temp_dir().join(format!(
        "daruma-graph-restore-{}.sqlite",
        TaskId::new().to_string().replace(':', "_")
    ));
    // The harness DB is sqlx's shared-cache memory URI; a bare target path
    // would inherit `mode=memory`, so name the file as an explicit URI.
    sqlx::Executor::execute(
        &app.pool,
        format!("VACUUM INTO 'file:{}?mode=rwc'", backup_path.display()).as_str(),
    )
    .await
    .unwrap();
    let backup_head = app.event_store().latest_seq().await.unwrap();
    println!(
        "TRACE backup taken at seq={backup_head} path={}",
        backup_path.display()
    );

    create_task(&app, &token, &project_id, "Secret merger rollout").await;
    sync_graph(&app).await;
    assert_eq!(
        graph_hits(&app, &token, "merger").await,
        ["Secret merger rollout"]
    );
    let live_cursor = app.state.workspace_graph.cursor().await.unwrap().unwrap();
    println!(
        "TRACE live graph cursor seq={} id={:?}",
        live_cursor.seq, live_cursor.event_id
    );
    assert!(live_cursor.seq > backup_head);

    // Restore: the server starts on the backup file with the old sidecar.
    let restored = daruma_storage::Db::open(&format!("sqlite:{}", backup_path.display()))
        .await
        .unwrap();
    restored.migrate().await.unwrap();
    let restored_store = daruma_storage::SqliteEventStore::new(restored.pool().clone());
    let restored_head = restored_store.latest_seq().await.unwrap();
    println!("TRACE restored log head seq={restored_head}");
    assert_eq!(
        restored_head, backup_head,
        "backup holds the pre-merger log"
    );
    let replayed =
        workspace_graph::catch_up_from_events(&app.state.workspace_graph, &restored_store)
            .await
            .unwrap();
    let cursor = app.state.workspace_graph.cursor().await.unwrap().unwrap();
    println!(
        "TRACE after restore catch-up replayed={replayed} cursor seq={}",
        cursor.seq
    );

    assert_eq!(
        replayed, backup_head,
        "graph replays the whole restored log"
    );
    assert_eq!(cursor.seq, backup_head);
    assert!(
        graph_hits(&app, &token, "merger").await.is_empty(),
        "rolled-back task must leave the graph"
    );
    assert_eq!(
        graph_hits(&app, &token, "budget").await,
        ["Quarterly budget review"]
    );
    let (_, status) = json_get(app.router.clone(), &token, "/v1/workspacegraph/status").await;
    println!("TRACE /v1/workspacegraph/status {status}");
    assert_eq!(status["last_event_seq"].as_u64(), Some(backup_head));

    restored.pool().close().await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", backup_path.display()));
    }
}
