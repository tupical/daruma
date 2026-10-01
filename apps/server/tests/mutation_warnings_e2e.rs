//! Advisory `MutationWarning`s: plan will not auto-close
//! (`plan_close_blocked_by_run`, `plan_draft_no_autoclose`) and an agent
//! taking more work while it still holds in-progress tasks
//! (`agent_has_open_tasks`). None of them blocks the operation.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::{test_app, TestApp};

async fn call(h: &TestApp, method: Method, uri: &str, body: Value) -> Value {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", h.admin_token))
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = h.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert!(status.is_success(), "{uri} failed: {v}");
    v
}

async fn post(h: &TestApp, uri: &str, body: Value) -> Value {
    call(h, Method::POST, uri, body).await
}

fn created_id(resp: &Value, event: &str, entity: &str) -> String {
    resp["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            (p.get("type")?.as_str()? == event)
                .then(|| p.get(entity)?.get("id")?.as_str().map(str::to_owned))?
        })
        .unwrap_or_else(|| panic!("{event} in {resp}"))
}

/// Plan with `n` tasks; activated unless `draft`. Returns (plan_id, task_ids).
async fn plan_with_tasks(h: &TestApp, n: usize, draft: bool) -> (String, Vec<String>) {
    let r = post(
        h,
        "/v1/commands",
        json!({"command":{"type":"create_project","title":"P"}}),
    )
    .await;
    let pid = created_id(&r, "project_created", "project");
    post(
        h,
        "/v1/plans",
        json!({"plan":{"project_id":pid,"title":"Plan","owner":{"kind":"user"}}}),
    )
    .await;
    let list = call(
        h,
        Method::GET,
        &format!("/v1/plans?project_id={pid}&status=all"),
        Value::Null,
    )
    .await;
    let plan_id = list[0]["id"].as_str().unwrap().to_owned();
    let mut tasks = vec![];
    for i in 0..n {
        let r = post(
            h,
            "/v1/commands",
            json!({"command":{"type":"create_task","task":{"title":format!("t{i}")}}}),
        )
        .await;
        let tid = created_id(&r, "task_created", "task");
        post(
            h,
            &format!("/v1/plans/{plan_id}/tasks"),
            json!({"task_id":tid}),
        )
        .await;
        tasks.push(tid);
    }
    if !draft {
        post(
            h,
            &format!("/v1/plans/{plan_id}/status"),
            json!({"status":"active"}),
        )
        .await;
    }
    (plan_id, tasks)
}

async fn complete(h: &TestApp, task_id: &str) -> Vec<Value> {
    let r = post(
        h,
        "/v1/commands",
        json!({"command":{"type":"complete_task","id":task_id}}),
    )
    .await;
    r["warnings"].as_array().cloned().unwrap_or_default()
}

fn codes(warnings: &[Value]) -> Vec<&str> {
    warnings
        .iter()
        .map(|w| w["code"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn last_task_with_open_run_warns_plan_close_blocked_by_run() {
    let h = test_app().await;
    let (plan_id, tasks) = plan_with_tasks(&h, 1, false).await;
    // drain-next opens a run and claims the task.
    let drained = post(&h, &format!("/v1/plans/{plan_id}/drain-next"), json!({})).await;
    assert_eq!(drained["task_id"], tasks[0]);

    let warnings = complete(&h, &tasks[0]).await;
    assert_eq!(
        codes(&warnings),
        ["plan_close_blocked_by_run"],
        "{warnings:?}"
    );
    let d = &warnings[0]["details"];
    assert_eq!(d["plan_id"], plan_id);
    assert_eq!(d["run_ids"].as_array().unwrap().len(), 1);
    assert_eq!(d["run_id"], d["run_ids"][0]);
}

#[tokio::test]
async fn last_task_in_draft_plan_warns_plan_draft_no_autoclose() {
    let h = test_app().await;
    let (plan_id, tasks) = plan_with_tasks(&h, 1, true).await;

    let warnings = complete(&h, &tasks[0]).await;
    assert_eq!(
        codes(&warnings),
        ["plan_draft_no_autoclose"],
        "{warnings:?}"
    );
    assert_eq!(warnings[0]["details"]["plan_id"], plan_id);
}

#[tokio::test]
async fn normal_completion_paths_carry_no_plan_warning() {
    let h = test_app().await;
    // Not the last task: plan stays open by design.
    let (_, tasks) = plan_with_tasks(&h, 2, false).await;
    assert!(complete(&h, &tasks[0]).await.is_empty());
    // Last task, active plan, no open run: the plan simply closes.
    let (plan_id, tasks) = plan_with_tasks(&h, 1, false).await;
    assert!(complete(&h, &tasks[0]).await.is_empty());
    let plan = call(
        &h,
        Method::GET,
        &format!("/v1/plans/{plan_id}"),
        Value::Null,
    )
    .await;
    assert_eq!(plan["plan"]["status"], "completed", "{plan}");
}

#[tokio::test]
async fn second_drain_and_claim_warn_agent_has_open_tasks() {
    let h = test_app().await;
    let (plan_id, tasks) = plan_with_tasks(&h, 3, false).await;
    let drain = format!("/v1/plans/{plan_id}/drain-next");

    let first = post(&h, &drain, json!({})).await;
    assert!(first.get("warnings").is_none(), "nothing open yet: {first}");

    let second = post(&h, &drain, json!({})).await;
    let w = &second["warnings"][0];
    assert_eq!(w["code"], "agent_has_open_tasks", "{second}");
    assert_eq!(w["details"]["count"], 1);
    assert_eq!(w["details"]["task_ids"], json!([first["task_id"]]));
    assert!(second["task_id"].is_string(), "operation not blocked");

    // Plain claim of the remaining task (claim alone does not start it).
    let claim = post(&h, "/v1/claims", json!({"task_id":tasks[2],"ttl_secs":60})).await;
    assert_eq!(claim["success"], true);
    assert_eq!(
        codes(claim["warnings"].as_array().unwrap()),
        ["agent_has_open_tasks"]
    );
    assert_eq!(claim["warnings"][0]["details"]["count"], 1);
}
