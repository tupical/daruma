//! Advisory `MutationWarning`s: plan will not auto-close
//! (`plan_close_blocked_by_run`, `plan_draft_no_autoclose`) and an agent
//! taking more work while it still holds in-progress tasks
//! (`agent_has_open_tasks`). None of them blocks the operation.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request},
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

/// New project; returns its id.
async fn new_project(h: &TestApp) -> String {
    let r = post(
        h,
        "/v1/commands",
        json!({"command":{"type":"create_project","title":"P"}}),
    )
    .await;
    created_id(&r, "project_created", "project")
}

/// Plan `title` in project `pid` with `n` tasks; activated unless `draft`.
/// Returns (plan_id, task_ids).
async fn plan_in(
    h: &TestApp,
    pid: &str,
    title: &str,
    n: usize,
    draft: bool,
) -> (String, Vec<String>) {
    post(
        h,
        "/v1/plans",
        json!({"plan":{"project_id":pid,"title":title,"owner":{"kind":"user"}}}),
    )
    .await;
    let list = call(
        h,
        Method::GET,
        &format!("/v1/plans?project_id={pid}&status=all"),
        Value::Null,
    )
    .await;
    let plan_id = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["title"] == title)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut tasks = vec![];
    for i in 0..n {
        let r = post(
            h,
            "/v1/commands",
            json!({"command":{"type":"create_task","task":{"title":format!("{title}-t{i}"),"project_id":pid}}}),
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

/// Plan with `n` tasks in a fresh project.
async fn plan_with_tasks(h: &TestApp, n: usize, draft: bool) -> (String, Vec<String>) {
    let pid = new_project(h).await;
    plan_in(h, &pid, "Plan", n, draft).await
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
async fn redrain_of_own_task_does_not_warn_about_itself() {
    let h = test_app().await;
    let (plan_id, _) = plan_with_tasks(&h, 3, false).await;
    let drain = format!("/v1/plans/{plan_id}/drain-next");

    let first = post(&h, &drain, json!({})).await;
    assert!(first.get("warnings").is_none(), "nothing open yet: {first}");

    // Resume-after-crash shape: drain again while still holding the first task.
    let second = post(&h, &drain, json!({})).await;
    assert!(second["task_id"].is_string(), "operation not blocked");
    if second["task_id"] == first["task_id"] {
        assert!(second.get("warnings").is_none(), "self-warning: {second}");
    } else {
        // Resolver handed out another task: the first one is genuinely open.
        let w = &second["warnings"][0];
        assert_eq!(w["code"], "agent_has_open_tasks", "{second}");
        assert_eq!(w["details"]["task_ids"], json!([first["task_id"]]));
    }
}

#[tokio::test]
async fn drain_of_other_plan_warns_about_first_task_not_the_new_one() {
    let h = test_app().await;
    let pid = new_project(&h).await;
    let (plan_a, _) = plan_in(&h, &pid, "A", 1, false).await;
    let (plan_b, _) = plan_in(&h, &pid, "B", 1, false).await;

    let a = post(&h, &format!("/v1/plans/{plan_a}/drain-next"), json!({})).await;
    let b = post(&h, &format!("/v1/plans/{plan_b}/drain-next"), json!({})).await;
    assert_ne!(a["task_id"], b["task_id"]);
    let w = &b["warnings"][0];
    assert_eq!(w["code"], "agent_has_open_tasks", "{b}");
    assert_eq!(w["details"]["count"], 1);
    assert_eq!(w["details"]["task_ids"], json!([a["task_id"]]));
}

#[tokio::test]
async fn project_ready_drain_warns_like_plan_drain() {
    let h = test_app().await;
    let pid = new_project(&h).await;
    let (plan_a, _) = plan_in(&h, &pid, "A", 1, false).await;
    plan_in(&h, &pid, "B", 1, false).await;

    let a = post(&h, &format!("/v1/plans/{plan_a}/drain-next"), json!({})).await;
    let next = post(&h, &format!("/v1/ready/drain?project_id={pid}"), json!({})).await;
    assert!(next["task_id"].is_string(), "operation not blocked: {next}");
    if next["task_id"] == a["task_id"] {
        assert!(next.get("warnings").is_none(), "self-warning: {next}");
    } else {
        let w = &next["warnings"][0];
        assert_eq!(w["code"], "agent_has_open_tasks", "{next}");
        assert_eq!(w["details"]["task_ids"], json!([a["task_id"]]));
    }
}

#[tokio::test]
async fn claim_warns_about_other_held_task_but_not_renewal() {
    let h = test_app().await;
    let (plan_id, tasks) = plan_with_tasks(&h, 2, false).await;
    let first = post(&h, &format!("/v1/plans/{plan_id}/drain-next"), json!({})).await;
    let held = first["task_id"].as_str().unwrap();
    let other = tasks.iter().find(|t| *t != held).unwrap();

    let claim = post(&h, "/v1/claims", json!({"task_id":other,"ttl_secs":60})).await;
    assert_eq!(claim["success"], true);
    assert_eq!(
        codes(claim["warnings"].as_array().unwrap()),
        ["agent_has_open_tasks"]
    );
    assert_eq!(claim["warnings"][0]["details"]["task_ids"], json!([held]));

    // Renewing the held task itself must not warn about itself.
    let renew = post(&h, "/v1/claims", json!({"task_id":held,"ttl_secs":60})).await;
    assert!(
        renew["warnings"].as_array().is_none_or(|w| w.is_empty()),
        "{renew}"
    );
}

/// The server advisory and core's real auto-close must agree on one scenario:
/// last task done in an Active plan with no run -> plan closes and no warning;
/// with an open run / in a Draft plan -> plan stays open and warning fires.
#[tokio::test]
async fn warnings_agree_with_core_autoclose() {
    let h = test_app().await;
    let status = |plan: Value| plan["plan"]["status"].as_str().unwrap().to_owned();
    for (draft, run, expect_warn) in [
        (false, false, false),
        (false, true, true),
        (true, false, true),
    ] {
        let (plan_id, tasks) = plan_with_tasks(&h, 1, draft).await;
        if run {
            post(&h, &format!("/v1/plans/{plan_id}/drain-next"), json!({})).await;
        }
        let warnings = complete(&h, &tasks[0]).await;
        let plan = call(
            &h,
            Method::GET,
            &format!("/v1/plans/{plan_id}"),
            Value::Null,
        )
        .await;
        let closed = status(plan) == "completed";
        assert_eq!(warnings.is_empty(), !expect_warn, "{warnings:?}");
        assert_eq!(closed, !expect_warn, "draft={draft} run={run}");
    }
}
