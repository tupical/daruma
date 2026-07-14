//! End-to-end tests for the Agent Operations read layer (VIZ-5):
//!
//!   GET /v1/sessions/active          → sessions with no `ended_at`
//!   GET /v1/claims[?project_id]      → live (non-expired) task claims
//!   GET /v1/work-units?project_id    → project-wide work-unit queue
//!
//! Acceptance: an active session/claim/work-unit is visible through the new
//! endpoints; ended/released/completed ones are not. Read-only viewer tokens
//! (RunRead/TaskRead, no AgentDispatch) can use every GET.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use daruma_auth::{Capabilities, Capability, ProjectFilter};
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::{mint_pat, test_app};

async fn post_json(app: &axum::Router, token: &str, uri: &str, body: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get_json(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn delete_json(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::DELETE)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn find_id(ev: &Value, ty: &str, entity: &str) -> String {
    ev["data"]
        .as_array()
        .expect("data array")
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            (p.get("type")?.as_str()? == ty)
                .then(|| p.get(entity)?.get("id")?.as_str().map(str::to_owned))
                .flatten()
        })
        .unwrap_or_else(|| panic!("{ty} event in {ev}"))
}

async fn create_project(app: &axum::Router, token: &str) -> String {
    let (_s, ev) = post_json(
        app,
        token,
        "/v1/commands",
        r#"{"command":{"type":"create_project","title":"Agent Ops"}}"#,
    )
    .await;
    find_id(&ev, "project_created", "project")
}

async fn create_task(app: &axum::Router, token: &str, project_id: &str) -> String {
    let body = json!({
        "command": {"type": "create_task", "task": {"title": "ops target", "project_id": project_id}}
    });
    let (_s, ev) = post_json(app, token, "/v1/commands", &body.to_string()).await;
    find_id(&ev, "task_created", "task")
}

async fn start_session(app: &axum::Router, token: &str, agent_id: &str) -> String {
    let body = json!({ "agent_id": agent_id }).to_string();
    let (s, resp) = post_json(app, token, "/v1/sessions", &body).await;
    assert_eq!(s, StatusCode::CREATED, "start session: {resp}");
    resp["data"]["id"].as_str().expect("session id").to_owned()
}

#[tokio::test]
async fn active_sessions_exclude_ended() {
    let h = test_app().await;
    let admin = &h.admin_token;

    let live = start_session(&h.router, admin, &uuid::Uuid::new_v4().to_string()).await;
    let done = start_session(&h.router, admin, &uuid::Uuid::new_v4().to_string()).await;
    let (s, resp) = post_json(&h.router, admin, &format!("/v1/sessions/{done}/end"), "{}").await;
    assert_eq!(s, StatusCode::OK, "end session: {resp}");

    let (s, resp) = get_json(&h.router, admin, "/v1/sessions/active").await;
    assert_eq!(s, StatusCode::OK, "{resp}");
    let sessions = resp["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "only the live session: {resp}");
    assert_eq!(sessions[0]["id"], live.as_str());
}

#[tokio::test]
async fn active_claims_scope_to_project_and_drop_on_release() {
    let h = test_app().await;
    let admin = &h.admin_token;
    let pid = create_project(&h.router, admin).await;
    let task = create_task(&h.router, admin, &pid).await;
    let agent = uuid::Uuid::new_v4().to_string();

    let body = json!({ "agent_id": agent, "task_id": task, "ttl_secs": 300 }).to_string();
    let (s, resp) = post_json(&h.router, admin, "/v1/claims", &body).await;
    assert_eq!(s, StatusCode::OK, "acquire: {resp}");

    let (s, resp) = get_json(&h.router, admin, &format!("/v1/claims?project_id={pid}")).await;
    assert_eq!(s, StatusCode::OK, "{resp}");
    let claims = resp["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 1, "{resp}");
    assert_eq!(claims[0]["task_id"], task.as_str());
    assert_eq!(claims[0]["agent_id"], agent.as_str());

    // A different project sees nothing.
    let other = create_project(&h.router, admin).await;
    let (_s, resp) = get_json(&h.router, admin, &format!("/v1/claims?project_id={other}")).await;
    assert_eq!(resp["claims"].as_array().unwrap().len(), 0, "{resp}");

    let (s, resp) = delete_json(&h.router, admin, &format!("/v1/claims/{agent}/{task}")).await;
    assert_eq!(s, StatusCode::OK, "release: {resp}");
    let (_s, resp) = get_json(&h.router, admin, "/v1/claims").await;
    assert_eq!(resp["claims"].as_array().unwrap().len(), 0, "{resp}");
}

#[tokio::test]
async fn project_work_units_list_and_status_filter() {
    let h = test_app().await;
    let admin = &h.admin_token;
    let pid = create_project(&h.router, admin).await;
    let task = create_task(&h.router, admin, &pid).await;

    for title in ["unit-a", "unit-b"] {
        let body = json!({ "work_unit": { "task_id": task, "title": title } }).to_string();
        let (s, resp) = post_json(&h.router, admin, "/v1/work-units", &body).await;
        assert_eq!(s, StatusCode::OK, "create unit: {resp}");
    }

    let (s, resp) = get_json(
        &h.router,
        admin,
        &format!("/v1/work-units?project_id={pid}"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{resp}");
    let units = resp["work_units"].as_array().unwrap();
    assert_eq!(units.len(), 2, "{resp}");

    // Complete one via drain (claims it) + complete; status filter narrows.
    let body = json!({ "task_id": task }).to_string();
    let (_s, drained) = post_json(&h.router, admin, "/v1/work-units/drain-next", &body).await;
    let unit_id = drained["work_unit"]["id"]
        .as_str()
        .expect("drained unit id");
    let (s, resp) = post_json(
        &h.router,
        admin,
        &format!("/v1/work-units/{unit_id}/complete"),
        "{}",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "complete: {resp}");

    let (_s, resp) = get_json(
        &h.router,
        admin,
        &format!("/v1/work-units?project_id={pid}&status=todo"),
    )
    .await;
    let todo = resp["work_units"].as_array().unwrap();
    assert_eq!(todo.len(), 1, "one unit still queued: {resp}");
    assert_ne!(todo[0]["id"], unit_id, "completed unit filtered out");

    // Unknown status → 400.
    let (s, _r) = get_json(
        &h.router,
        admin,
        &format!("/v1/work-units?project_id={pid}&status=bogus"),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn viewer_token_reads_all_agent_ops_endpoints() {
    let h = test_app().await;
    let admin = &h.admin_token;
    let pid = create_project(&h.router, admin).await;
    let agent = uuid::Uuid::new_v4().to_string();
    start_session(&h.router, admin, &agent).await;

    let caps: Capabilities = [Capability::RunRead, Capability::TaskRead].into();
    let (viewer, _) = mint_pat(&h.auth_store(), caps, ProjectFilter::All).await;

    for uri in [
        "/v1/sessions/active".to_string(),
        format!("/v1/sessions?agent_id={agent}"),
        "/v1/claims".to_string(),
        format!("/v1/work-units?project_id={pid}"),
    ] {
        let (s, resp) = get_json(&h.router, &viewer, &uri).await;
        assert_eq!(s, StatusCode::OK, "viewer must read {uri}: {resp}");
    }

    // A token with neither RunRead nor AgentDispatch is still rejected.
    let (nobody, _) = mint_pat(
        &h.auth_store(),
        [Capability::CommentRead].into(),
        ProjectFilter::All,
    )
    .await;
    let (s, _r) = get_json(&h.router, &nobody, "/v1/sessions/active").await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}
