//! End-to-end test: file/path work leases prevent two agents from editing the
//! same files, and auto-release when the task closes.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::Value;
use tower::ServiceExt;

mod common;
use common::test_app;

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

async fn create_project(app: &axum::Router, token: &str) -> String {
    let (_s, ev) = post_json(
        app,
        token,
        "/v1/commands",
        r#"{"command":{"type":"create_project","title":"Lease Project"}}"#,
    )
    .await;
    find_id(&ev, "project_created", "project")
}

async fn create_task(app: &axum::Router, token: &str, title: &str) -> String {
    let body = format!(r#"{{"command":{{"type":"create_task","task":{{"title":"{title}"}}}}}}"#);
    let (_s, ev) = post_json(app, token, "/v1/commands", &body).await;
    find_id(&ev, "task_created", "task")
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

#[tokio::test]
async fn overlapping_lease_is_rejected_and_releases_on_close() {
    let h = test_app().await;
    let admin = &h.admin_token;
    let pid = create_project(&h.router, admin).await;
    let task_a = create_task(&h.router, admin, "agent A work").await;
    let task_b = create_task(&h.router, admin, "agent B work").await;
    let agent_a = uuid::Uuid::new_v4().to_string();
    let agent_b = uuid::Uuid::new_v4().to_string();

    // Agent A reserves a subtree.
    let body = format!(
        r#"{{"agent_id":"{agent_a}","task_id":"{task_a}","project_id":"{pid}","paths":["crates/storage/src"]}}"#
    );
    let (s, r) = post_json(&h.router, admin, "/v1/leases", &body).await;
    assert_eq!(s, StatusCode::OK, "reserve A failed: {r}");
    assert_eq!(r["data"]["reserved"], true, "A should reserve: {r}");

    // Agent B reserves an overlapping descendant → conflict, holder = A.
    let body = format!(
        r#"{{"agent_id":"{agent_b}","task_id":"{task_b}","project_id":"{pid}","paths":["crates/storage/src/claim_repo.rs"]}}"#
    );
    let (s, r) = post_json(&h.router, admin, "/v1/leases", &body).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["data"]["reserved"], false, "B must be blocked: {r}");
    assert_eq!(r["data"]["holder"], agent_a, "holder must be A: {r}");

    // Agent B reserves a non-overlapping subtree → success.
    let body = format!(
        r#"{{"agent_id":"{agent_b}","task_id":"{task_b}","project_id":"{pid}","paths":["crates/core/src"]}}"#
    );
    let (s, r) = post_json(&h.router, admin, "/v1/leases", &body).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r["data"]["reserved"], true,
        "B non-overlap should reserve: {r}"
    );

    // Active work backlog shows both leases.
    let (s, r) = get_json(&h.router, admin, &format!("/v1/leases?project_id={pid}")).await;
    assert_eq!(s, StatusCode::OK, "active_work failed: {r}");
    assert_eq!(
        r["leases"].as_array().unwrap().len(),
        2,
        "two live leases: {r}"
    );

    // Close task A → A's lease auto-releases; only B's remains.
    let close = format!(r#"{{"command":{{"type":"set_status","id":"{task_a}","status":"done"}}}}"#);
    let (s, r) = post_json(&h.router, admin, "/v1/commands", &close).await;
    assert_eq!(s, StatusCode::OK, "close task A failed: {r}");

    let (s, r) = get_json(&h.router, admin, &format!("/v1/leases?project_id={pid}")).await;
    assert_eq!(s, StatusCode::OK);
    let leases = r["leases"].as_array().unwrap();
    assert_eq!(leases.len(), 1, "only B's lease should remain: {r}");
    assert_eq!(
        leases[0]["agent_id"], agent_b,
        "remaining lease must be B's: {r}"
    );
}
