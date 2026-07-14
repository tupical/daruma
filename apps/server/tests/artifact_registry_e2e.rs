//! End-to-end integration tests for the Artifact Registry HTTP surface.
//!
//! Read paths (`GET /v1/artifacts`, `GET /v1/artifacts/{id}/impact`) that
//! predate the write layer seed the projection directly via
//! `ArtifactRepo::apply_event`. The newer write paths (`POST /v1/artifacts`,
//! `POST /v1/artifacts/{id}/status`) exercise the real command bus → event →
//! projection round-trip.

use axum::http::StatusCode;
use daruma_domain::{
    Actor, Artifact, ArtifactRelation, ArtifactRelationKind, ArtifactStatus, LeaseMode, WorkLease,
};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{
    AgentId, ArtifactId, ArtifactRelationId, ProjectId, TaskId, WorkLeaseId,
};

mod common;
use common::{json_get, json_post, test_app};

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
        Event::ArtifactRegistered { artifact: a.clone() },
    );
    // Projection consumed by GET /v1/artifacts.
    app.state.artifacts.apply_event(&env).await.unwrap();
    // WorkspaceGraph consumed by the impact traversal.
    app.state.workspace_graph.apply_event(&env).await.unwrap();
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
    assert_eq!(item["id"].as_str().unwrap(), active.id.as_uuid().to_string());
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
    let (status, body) =
        json_get(app.router.clone(), &token, "/v1/artifacts?kind=file").await;
    assert_eq!(status, StatusCode::OK);
    let items = body["artifacts"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["uri"].as_str().unwrap(), "file://README.md");
    assert!(items[0]["current_holder_agent_id"].is_null());
}

#[tokio::test]
async fn impact_returns_neighborhood_for_known_artifact() {
    let app = test_app().await;
    let token = app.admin_token.clone();

    let from = artifact("artifact://svc/auth", None);
    let to = artifact("contract://auth@v1", None);
    seed_artifact(&app, &from).await;
    seed_artifact(&app, &to).await;

    // from --DependsOn--> to, seeded into both projections.
    let rel = ArtifactRelation {
        id: ArtifactRelationId::new(),
        from_id: from.id,
        to_id: to.id,
        kind: ArtifactRelationKind::DependsOn,
        created_at: chrono::Utc::now(),
    };
    let rel_env = EventEnvelope::new(
        Actor::user(),
        Event::ArtifactRelationAdded {
            relation: rel.clone(),
        },
    );
    app.state.artifacts.apply_event(&rel_env).await.unwrap();
    app.state
        .workspace_graph
        .apply_event(&rel_env)
        .await
        .unwrap();

    let (status, body) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/artifacts/{}/impact", from.id),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let nodes = body["nodes"].as_array().expect("nodes array");
    let edges = body["edges"].as_array().expect("edges array");
    let from_node = format!("artifact:{}", from.id);
    let to_node = format!("artifact:{}", to.id);
    assert!(
        nodes.iter().any(|n| n["id"].as_str() == Some(&from_node)),
        "root artifact node present: {body}"
    );
    assert!(
        nodes.iter().any(|n| n["id"].as_str() == Some(&to_node)),
        "downstream artifact node present: {body}"
    );
    assert!(
        edges
            .iter()
            .any(|e| e["kind"].as_str() == Some("ArtDependsOn")),
        "dependency edge present: {body}"
    );
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
    assert_eq!(
        items[0]["uri"].as_str().unwrap(),
        "artifact://api/payments"
    );
    assert_eq!(items[0]["title"].as_str().unwrap(), "Payments API");
    assert_eq!(items[0]["status"].as_str().unwrap(), "pending");
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
