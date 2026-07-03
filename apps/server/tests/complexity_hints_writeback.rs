//! `POST /v1/complexity-hints` — projection write-back contract.
//!
//! The planning layer returns pure hint drafts (no `batch_id` /
//! `generated_at`); core assigns persistence identity and upserts the
//! `task_complexity_hints` projection. Covers: happy path (identity
//! assignment + clamping), unknown task rejection, empty-batch rejection.

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
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Create a task via /v1/commands and return its id string.
async fn create_task(app: &axum::Router, token: &str, title: &str) -> String {
    let (s, ev) = post_json(
        app,
        token,
        "/v1/commands",
        &format!(r#"{{"command":{{"type":"create_task","task":{{"title":"{title}"}}}}}}"#),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "create_task failed: {ev}");
    ev["data"]
        .as_array()
        .expect("data must be array")
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
async fn writeback_assigns_batch_identity_and_clamps() {
    let h = test_app().await;
    let t1 = create_task(&h.router, &h.admin_token, "hint target one").await;
    let t2 = create_task(&h.router, &h.admin_token, "hint target two").await;

    // score 0 must clamp to 1; score 99 to 10; recommended_subtasks 99 to 20.
    let body = format!(
        r#"{{"hints":[
            {{"task_id":"{t1}","score":0,"recommended_subtasks":99,"expansion_hint":"split by module","reasoning":"broad"}},
            {{"task_id":"{t2}","score":99}}
        ]}}"#
    );
    let (s, resp) = post_json(&h.router, &h.admin_token, "/v1/complexity-hints", &body).await;
    assert_eq!(s, StatusCode::OK, "expected 200: {resp}");

    let batch_id = resp["batch_id"].as_str().expect("batch_id assigned");
    let hints = resp["hints"].as_array().expect("hints array");
    assert_eq!(hints.len(), 2);
    for h in hints {
        assert_eq!(h["batch_id"].as_str().unwrap(), batch_id);
        assert!(h["generated_at"].is_string(), "generated_at assigned: {h}");
    }
    assert_eq!(hints[0]["score"], 1);
    assert_eq!(hints[0]["recommended_subtasks"], 20);
    assert_eq!(hints[1]["score"], 10);

    // The projection is actually written, not just echoed.
    let t1_id: daruma_shared::TaskId = t1.parse().unwrap();
    let stored = h
        .state
        .complexity_hints
        .get(t1_id)
        .await
        .unwrap()
        .expect("hint row persisted");
    assert_eq!(stored.batch_id, batch_id);
    assert_eq!(stored.score, 1);
}

#[tokio::test]
async fn writeback_rejects_unknown_task_ids() {
    let h = test_app().await;
    let real = create_task(&h.router, &h.admin_token, "real task").await;
    // Wire form (bare uuid): TaskId serde is the derived inner-uuid repr,
    // Display's "tsk_"-prefixed form is not valid JSON input here.
    let ghost = serde_json::to_value(daruma_shared::TaskId::new()).unwrap();
    let ghost = ghost.as_str().unwrap().to_owned();

    let body = format!(
        r#"{{"hints":[
            {{"task_id":"{real}","score":5}},
            {{"task_id":"{ghost}","score":5}}
        ]}}"#
    );
    let (s, resp) = post_json(&h.router, &h.admin_token, "/v1/complexity-hints", &body).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "expected 400: {resp}");

    // Atomic: the valid row must not have been written either.
    let real_id: daruma_shared::TaskId = real.parse().unwrap();
    assert!(h.state.complexity_hints.get(real_id).await.unwrap().is_none());
}

#[tokio::test]
async fn writeback_rejects_empty_batch() {
    let h = test_app().await;
    let (s, resp) = post_json(
        &h.router,
        &h.admin_token,
        "/v1/complexity-hints",
        r#"{"hints":[]}"#,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "expected 400: {resp}");
}
