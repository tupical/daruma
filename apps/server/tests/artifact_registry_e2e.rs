//! End-to-end integration tests for the Artifact Registry HTTP surface.
//!
//! Read paths (`GET /v1/artifacts`, `GET /v1/artifacts/{id}/impact`) that
//! predate the write layer seed the projection directly via
//! `ArtifactRepo::apply_event`. The newer write paths (`POST /v1/artifacts`,
//! `POST /v1/artifacts/{id}/status`) exercise the real command bus → event →
//! projection round-trip.

use axum::http::StatusCode;
use daruma_domain::{Actor, Artifact, ArtifactRelationKind, ArtifactStatus, LeaseMode, WorkLease};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{AgentId, ArtifactId, ProjectId, TaskId, WorkLeaseId};
use serde_json::{json, Value};

mod common;
use common::{json_get, json_post, spawn_server, test_app};

fn artifact(uri: &str, project_id: Option<ProjectId>) -> Artifact {
    let now = chrono::Utc::now();
    Artifact {
        id: ArtifactId::new(),
        uri: uri.to_string(),
        title: format!("Artifact {uri}"),
        description: String::new(),
        status: ArtifactStatus::Pending,
        owner_agent_id: None,
        task_id: None,
        project_id,
        version: None,
        last_write_token: None,
        created_at: now,
        updated_at: now,
    }
}

async fn seed_artifact(app: &common::TestApp, a: &Artifact) {
    let env = EventEnvelope::new(
        Actor::user(),
        Event::ArtifactRegistered {
            artifact: a.clone(),
        },
    );
    // Projection consumed by GET /v1/artifacts.
    app.state.artifacts.apply_event(&env).await.unwrap();
    // WorkspaceGraph consumed by the impact traversal.
    app.state.workspace_graph.apply_event(&env).await.unwrap();
}

async fn mcp_tool(addr: &std::net::SocketAddr, token: &str, name: &str, arguments: Value) -> Value {
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/mcp?profile=full"))
        .bearer_auth(token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{name} HTTP failure");
    let body: Value = response.json().await.unwrap();
    assert!(body["error"].is_null(), "{name} failed: {body}");
    serde_json::from_str(body["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

async fn wait_for_relation_edge(
    addr: &std::net::SocketAddr,
    token: &str,
    artifact_id: ArtifactId,
    present: bool,
) -> Value {
    let mut last = Value::Null;
    for _ in 0..100 {
        let impact = mcp_tool(
            addr,
            token,
            "daruma_artifact_impact",
            json!({ "artifact_id": artifact_id }),
        )
        .await;
        let found = impact["edges"]
            .as_array()
            .is_some_and(|edges| edges.iter().any(|edge| edge["kind"] == "ArtDependsOn"));
        if found == present {
            return impact;
        }
        last = impact;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("artifact relation edge did not reach expected state: present={present}, last={last}")
}

#[tokio::test]
async fn list_filters_by_project_and_status() {
    let app = test_app().await;
    let token = app.admin_token.clone();
    let project = ProjectId::new();

    // active (project) / pending (project) / pending (no project)
    let active = artifact("artifact://api/users", Some(project));
    let pending = artifact("artifact://api/orders", Some(project));
    let loose = artifact("file://README.md", None);
    seed_artifact(&app, &active).await;
    seed_artifact(&app, &pending).await;
    seed_artifact(&app, &loose).await;

    // Flip `active` to status=active.
    let flip = EventEnvelope::new(
        Actor::user(),
        Event::ArtifactStatusChanged {
            artifact_id: active.id,
            from: ArtifactStatus::Pending,
            to: ArtifactStatus::Active,
            at: chrono::Utc::now(),
        },
    );
    app.state.artifacts.apply_event(&flip).await.unwrap();

    // Attach an active work-lease to the `active` artifact uri → current holder.
    let holder = AgentId::new();
    let now = chrono::Utc::now();
    let lease = WorkLease {
        id: WorkLeaseId::new(),
        agent_id: holder,
        task_id: TaskId::new(),
        project_id: Some(project),
        path_glob: active.uri.clone(),
        target_uri: Some(active.uri.clone()),
        mode: LeaseMode::Exclusive,
        fencing_token: Some(1),
        acquired_at: now,
        expires_at: now + chrono::Duration::hours(1),
    };
    app.state.work_leases.apply_reserved(&lease).await.unwrap();

    // Filter: project + status=active → only `active`.
    let (status, body) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/artifacts?project_id={project}&status=active"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["artifacts"].as_array().unwrap();
    assert_eq!(items.len(), 1, "expected only the active artifact: {body}");
    let item = &items[0];
    // Ids serialize as bare UUIDs (`#[serde(transparent)]`), not the
    // prefixed Display form.
    assert_eq!(
        item["id"].as_str().unwrap(),
        active.id.as_uuid().to_string()
    );
    assert_eq!(item["uri"].as_str().unwrap(), "artifact://api/users");
    assert_eq!(item["kind"].as_str().unwrap(), "artifact");
    assert_eq!(item["status"].as_str().unwrap(), "active");
    assert_eq!(
        item["project_id"].as_str().unwrap(),
        project.as_uuid().to_string()
    );
    assert_eq!(
        item["current_holder_agent_id"].as_str().unwrap(),
        holder.as_uuid().to_string(),
        "derived lease holder should be joined in"
    );
    // owner is decoupled from the lease holder → null here.
    assert!(item["owner_agent_id"].is_null());

    // Whole project scope → both project artifacts (loose one excluded).
    let (status, body) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/artifacts?project_id={project}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["artifacts"].as_array().unwrap().len(), 2);

    // kind filter derives from the URI scheme.
    let (status, body) = json_get(app.router.clone(), &token, "/v1/artifacts?kind=file").await;
    assert_eq!(status, StatusCode::OK);
    let items = body["artifacts"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["uri"].as_str().unwrap(), "file://README.md");
    assert!(items[0]["current_holder_agent_id"].is_null());
}

#[tokio::test]
async fn relation_add_remove_flows_through_command_bus_and_mcp_impact() {
    let app = test_app().await;
    let token = app.admin_token.clone();
    let addr = spawn_server(&app).await;

    let from = artifact("artifact://svc/auth", None);
    let to = artifact("contract://auth@v1", None);
    seed_artifact(&app, &from).await;
    seed_artifact(&app, &to).await;

    let relation = json!({
        "from": from.id,
        "to": to.id,
        "kind": "depends_on"
    });
    let (status, body) = json_post(
        app.router.clone(),
        &token,
        "/v1/artifact-relations",
        &relation.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "relation add failed: {body}");
    let projected = app.state.artifacts.relations_for(from.id).await.unwrap();
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].kind, ArtifactRelationKind::DependsOn);

    let impact = wait_for_relation_edge(&addr, &token, from.id, true).await;
    let nodes = impact["nodes"].as_array().expect("nodes array");
    let from_node = format!("artifact:{}", from.id);
    let to_node = format!("artifact:{}", to.id);
    assert!(
        nodes.iter().any(|n| n["id"].as_str() == Some(&from_node)),
        "root artifact node present: {impact}"
    );
    assert!(
        nodes.iter().any(|n| n["id"].as_str() == Some(&to_node)),
        "downstream artifact node present: {impact}"
    );

    let (status, body) = json_post(
        app.router.clone(),
        &token,
        "/v1/artifact-relations/remove",
        &relation.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "relation remove failed: {body}");
    assert!(app
        .state
        .artifacts
        .relations_for(from.id)
        .await
        .unwrap()
        .is_empty());
    wait_for_relation_edge(&addr, &token, from.id, false).await;
}

#[tokio::test]
async fn register_via_http_then_list_sees_artifact() {
    let app = test_app().await;
    let token = app.admin_token.clone();
    let project = ProjectId::new();

    // POST /v1/artifacts — bare NewArtifact body (no wrapper), matching the
    // daruma_artifact_register MCP wire format.
    let (status, body) = json_post(
        app.router.clone(),
        &token,
        "/v1/artifacts",
        &format!(
            r#"{{"uri":"artifact://api/payments","title":"Payments API","description":"billing","project_id":"{}"}}"#,
            project.as_uuid()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register failed: {body}");
    assert!(body["success"].as_bool().unwrap_or(false));
    let new_id = body["data"]["artifact"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        body["data"]["artifact"]["status"].as_str().unwrap(),
        "pending"
    );

    // GET /v1/artifacts — the registered artifact is visible via the projection.
    let (status, body) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/artifacts?project_id={project}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["artifacts"].as_array().unwrap();
    assert_eq!(items.len(), 1, "expected registered artifact: {body}");
    assert_eq!(items[0]["id"].as_str().unwrap(), new_id);
    assert_eq!(items[0]["uri"].as_str().unwrap(), "artifact://api/payments");
    assert_eq!(items[0]["title"].as_str().unwrap(), "Payments API");
    assert_eq!(items[0]["status"].as_str().unwrap(), "pending");
}

#[tokio::test]
async fn update_and_deprecate_via_http_are_projected() {
    let app = test_app().await;
    let token = app.admin_token.clone();
    let artifact = artifact("artifact://svc/catalog", None);
    seed_artifact(&app, &artifact).await;

    let (status, body) = json_post(
        app.router.clone(),
        &token,
        &format!("/v1/artifacts/{}/update", artifact.id),
        r#"{"title":"Catalog v2","description":"updated"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "artifact update failed: {body}");
    let projected = app.state.artifacts.get(artifact.id).await.unwrap().unwrap();
    assert_eq!(projected.title, "Catalog v2");
    assert_eq!(projected.description, "updated");

    for _ in 0..2 {
        let (status, body) = json_post(
            app.router.clone(),
            &token,
            &format!("/v1/artifacts/{}/deprecate", artifact.id),
            "{}",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "artifact deprecation failed: {body}"
        );
    }
    assert_eq!(
        app.state
            .artifacts
            .get(artifact.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        ArtifactStatus::Deprecated
    );
}

#[tokio::test]
async fn commit_write_rejects_stale_fencing_token() {
    let app = test_app().await;
    let token = app.admin_token.clone();
    let artifact = artifact("artifact://svc/fenced", None);
    seed_artifact(&app, &artifact).await;

    let fencing_token = match app
        .state
        .work_leases
        .try_reserve_targets(
            app.admin_agent_id,
            TaskId::new(),
            None,
            vec![artifact.uri.clone()],
            LeaseMode::Exclusive,
            chrono::Duration::minutes(5),
        )
        .await
        .unwrap()
    {
        daruma_storage::ReserveOutcome::Reserved { leases } => {
            leases[0].fencing_token.expect("fencing token")
        }
        _ => panic!("artifact lease was not reserved"),
    };

    let (status, _) = json_post(
        app.router.clone(),
        &token,
        &format!("/v1/artifacts/{}/commit-write", artifact.id),
        &json!({ "fencing_token": fencing_token - 1, "version": "v1" }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(app
        .state
        .artifacts
        .get(artifact.id)
        .await
        .unwrap()
        .unwrap()
        .last_write_token
        .is_none());

    let (status, body) = json_post(
        app.router.clone(),
        &token,
        &format!("/v1/artifacts/{}/commit-write", artifact.id),
        &json!({ "fencing_token": fencing_token, "version": "v1" }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "valid commit failed: {body}");
    let projected = app.state.artifacts.get(artifact.id).await.unwrap().unwrap();
    assert_eq!(projected.last_write_token, Some(fencing_token));
    assert_eq!(projected.version.as_deref(), Some("v1"));
    assert_eq!(projected.status, ArtifactStatus::Committed);
}

#[tokio::test]
async fn status_change_via_http_is_reflected_in_list() {
    let app = test_app().await;
    let token = app.admin_token.clone();
    let project = ProjectId::new();

    let (status, body) = json_post(
        app.router.clone(),
        &token,
        "/v1/artifacts",
        &format!(
            r#"{{"uri":"artifact://svc/queue","title":"Queue","project_id":"{}"}}"#,
            project.as_uuid()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register failed: {body}");
    let id = body["data"]["artifact"]["id"].as_str().unwrap().to_string();

    // POST /v1/artifacts/{id}/status — flip to active.
    let (status, body) = json_post(
        app.router.clone(),
        &token,
        &format!("/v1/artifacts/{id}/status"),
        r#"{"status":"active"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "status change failed: {body}");
    assert!(body["success"].as_bool().unwrap_or(false));

    // GET reflects the new status.
    let (status, body) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/artifacts?project_id={project}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["artifacts"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["status"].as_str().unwrap(), "active");

    // Same-status change is a no-op (still 200, no error).
    let (status, _) = json_post(
        app.router.clone(),
        &token,
        &format!("/v1/artifacts/{id}/status"),
        r#"{"status":"active"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn duplicate_uri_registration_conflicts() {
    let app = test_app().await;
    let token = app.admin_token.clone();

    let body = r#"{"uri":"artifact://api/inventory","title":"Inventory"}"#;
    let (status, first) = json_post(app.router.clone(), &token, "/v1/artifacts", body).await;
    assert_eq!(status, StatusCode::OK, "first register failed: {first}");

    // Re-registering the same uri is rejected at the command layer (409) so the
    // existing row — and its relations — cannot be delete-then-inserted away.
    let (status, _) = json_post(app.router.clone(), &token, "/v1/artifacts", body).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Exactly one artifact survives; the original was not clobbered.
    let (status, list) = json_get(app.router.clone(), &token, "/v1/artifacts").await;
    assert_eq!(status, StatusCode::OK);
    let items = list["artifacts"].as_array().unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|a| a["uri"].as_str() == Some("artifact://api/inventory"))
            .count(),
        1
    );
}

#[tokio::test]
async fn impact_unknown_artifact_is_404() {
    let app = test_app().await;
    let token = app.admin_token.clone();

    let unknown = ArtifactId::new();
    let (status, _) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/artifacts/{unknown}/impact"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
