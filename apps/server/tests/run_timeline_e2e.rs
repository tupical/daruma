//! End-to-end tests for the read-only Run timeline HTTP surface.
//!
//! Exercises the real command path (`POST /v1/runs`, `.../step/start`,
//! `.../step/finish`) so the `run_steps` projection (migration 0049) is
//! populated by the command → event → projection round-trip, then reads it
//! back via `GET /v1/runs/{id}`, `GET /v1/plans/{id}/runs`, and
//! `GET /v1/runs/{id}/timeline`.

use axum::http::StatusCode;
use daruma_shared::{RunId, TaskId};

mod common;
use common::{json_get, json_post, test_app};

/// Create a project and a plan via `/v1/commands`, returning the plan id.
/// `StartRun` validates that the referenced plan exists, so runs need a real
/// plan behind them.
async fn create_plan(app: &common::TestApp, token: &str) -> String {
    let (status, ev) = json_post(
        app.router.clone(),
        token,
        "/v1/commands",
        r#"{"command":{"type":"create_project","title":"Timeline Project"}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create_project failed: {ev}");
    let project_id = ev["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            (p.get("type")?.as_str()? == "project_created")
                .then(|| p["project"]["id"].as_str().unwrap().to_owned())
        })
        .expect("project_created payload");

    let (status, ev) = json_post(
        app.router.clone(),
        token,
        "/v1/commands",
        &format!(
            r#"{{"command":{{"type":"create_plan","plan":{{"project_id":"{project_id}","title":"Timeline Plan","owner":{{"kind":"user"}}}}}}}}"#
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create_plan failed: {ev}");
    let plan_id = ev["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            (p.get("type")?.as_str()? == "plan_created")
                .then(|| p["plan"]["id"].as_str().unwrap().to_owned())
        })
        .expect("plan_created payload");

    // A run can only start on an Active plan (plans are created Draft).
    let (status, r) = json_post(
        app.router.clone(),
        token,
        &format!("/v1/plans/{plan_id}/status"),
        r#"{"status":"active"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "activate plan failed: {r}");

    plan_id
}

/// Start a run with two steps (one `Done`, one `Failed`) and a note, then
/// assert the timeline carries both steps with their full outcome structure.
#[tokio::test]
async fn timeline_reports_steps_notes_and_outcomes() {
    let app = test_app().await;
    let token = app.admin_token.clone();
    let agent = app.admin_agent_id;
    let plan_id = create_plan(&app, &token).await;
    let task_ok = TaskId::new();
    let task_fail = TaskId::new();

    // Start the run.
    let (status, body) = json_post(
        app.router.clone(),
        &token,
        "/v1/runs",
        &format!(
            r#"{{"plan_id":"{}","agent_id":"{}"}}"#,
            plan_id,
            agent.as_uuid()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "start run failed: {body}");
    let run_id = body["data"]["run_id"].as_str().unwrap().to_string();

    // Step 1: start → finish Done.
    let (status, _) = json_post(
        app.router.clone(),
        &token,
        &format!("/v1/runs/{run_id}/step/start"),
        &format!(r#"{{"task_id":"{}"}}"#, task_ok.as_uuid()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = json_post(
        app.router.clone(),
        &token,
        &format!("/v1/runs/{run_id}/step/finish"),
        &format!(
            r#"{{"task_id":"{}","outcome":{{"kind":"done"}}}}"#,
            task_ok.as_uuid()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Step 2: start → finish Failed{reason}.
    let (status, _) = json_post(
        app.router.clone(),
        &token,
        &format!("/v1/runs/{run_id}/step/start"),
        &format!(r#"{{"task_id":"{}"}}"#, task_fail.as_uuid()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = json_post(
        app.router.clone(),
        &token,
        &format!("/v1/runs/{run_id}/step/finish"),
        &format!(
            r#"{{"task_id":"{}","outcome":{{"kind":"failed","reason":"out of context"}}}}"#,
            task_fail.as_uuid()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Attach a note so the timeline carries the journal too.
    let (status, _) = json_post(
        app.router.clone(),
        &token,
        &format!("/v1/runs/{run_id}/notes"),
        r#"{"body":"kicked off the run"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // ── GET timeline ────────────────────────────────────────────────────────
    let (status, body) =
        json_get(app.router.clone(), &token, &format!("/v1/runs/{run_id}/timeline")).await;
    assert_eq!(status, StatusCode::OK, "timeline failed: {body}");

    assert_eq!(body["run"]["id"].as_str().unwrap(), run_id);

    let steps = body["steps"].as_array().expect("steps array");
    assert_eq!(steps.len(), 2, "expected two steps: {body}");

    // Steps are ordered by start; step 1 = Done on task_ok.
    let s0 = &steps[0];
    assert_eq!(s0["task_id"].as_str().unwrap(), task_ok.as_uuid().to_string());
    assert!(s0["started_at"].is_string());
    assert!(s0["finished_at"].is_string());
    // outcome is a nested JSON object, not an escaped string.
    assert!(s0["outcome"].is_object(), "outcome should be an object: {s0}");
    assert_eq!(s0["outcome"]["kind"].as_str().unwrap(), "done");

    // Step 2 = Failed{reason} on task_fail — reason must survive.
    let s1 = &steps[1];
    assert_eq!(
        s1["task_id"].as_str().unwrap(),
        task_fail.as_uuid().to_string()
    );
    assert_eq!(s1["outcome"]["kind"].as_str().unwrap(), "failed");
    assert_eq!(
        s1["outcome"]["reason"].as_str().unwrap(),
        "out of context",
        "Failed reason must not be lost: {s1}"
    );

    // Notes are present.
    let notes = body["notes"].as_array().expect("notes array");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["body"].as_str().unwrap(), "kicked off the run");
}

/// `GET /v1/runs/{id}` returns the run; `GET /v1/plans/{id}/runs` lists it.
#[tokio::test]
async fn get_run_and_list_plan_runs() {
    let app = test_app().await;
    let token = app.admin_token.clone();
    let agent = app.admin_agent_id;
    let plan_id = create_plan(&app, &token).await;

    let (status, body) = json_post(
        app.router.clone(),
        &token,
        "/v1/runs",
        &format!(
            r#"{{"plan_id":"{}","agent_id":"{}"}}"#,
            plan_id,
            agent.as_uuid()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "start run failed: {body}");
    let run_id = body["data"]["run_id"].as_str().unwrap().to_string();

    // GET /v1/runs/{id}
    let (status, body) = json_get(app.router.clone(), &token, &format!("/v1/runs/{run_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["run"]["id"].as_str().unwrap(), run_id);
    assert_eq!(body["run"]["status"].as_str().unwrap(), "active");

    // GET /v1/plans/{id}/runs
    let (status, body) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/plans/{plan_id}/runs"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let runs = body["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["id"].as_str().unwrap(), run_id);
}

/// Unknown run id → 404 on both single-run reads.
#[tokio::test]
async fn timeline_and_get_unknown_run_is_404() {
    let app = test_app().await;
    let token = app.admin_token.clone();
    let unknown = RunId::new();

    let (status, _) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/runs/{unknown}/timeline"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) =
        json_get(app.router.clone(), &token, &format!("/v1/runs/{unknown}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
