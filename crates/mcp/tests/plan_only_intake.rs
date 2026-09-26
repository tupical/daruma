//! Plan-only intake на MCP-поверхности (ADR-0007, item 3 разбивки).
//!
//! Verifies:
//!   1. `create_paths_return_bridge_error` — daruma_create / daruma_capture /
//!      daruma_capture_batch убраны: мост называет замену, HTTP не дёргается.
//!   2. `plan_materialize_posts_materialize_plan_command` — daruma_plan_materialize
//!      шлёт POST /v1/commands с {"type":"materialize_plan", plan, tasks}.
//!   3. `plan_materialize_requires_tasks` — пустой/отсутствующий `tasks` — ошибка
//!      до какого-либо запроса.

use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, http::StatusCode, routing::any, Router};
use daruma_mcp::tools::call_tool;
use daruma_mcp::ApiClient;
use serde_json::{json, Value};

#[derive(Debug, Clone)]
struct Captured {
    path: String,
    body: Value,
}

/// Router that records every request (any HTTP method) and returns 200 `{}`.
fn recording_router(capture: Arc<Mutex<Vec<Captured>>>) -> Router {
    Router::new().fallback(any(move |req: Request<Body>| {
        let capture = capture.clone();
        async move {
            let path = req.uri().path().to_string();
            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .unwrap_or_default();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            capture.lock().unwrap().push(Captured { path, body });
            (StatusCode::OK, axum::Json(json!({})))
        }
    }))
}

async fn with_recording_server(tool: &str, args: Value) -> (anyhow::Result<Value>, Vec<Captured>) {
    use tokio::net::TcpListener;

    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let router = recording_router(captured.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    let server_handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let client = ApiClient::new(base, "test-token");
    let result = call_tool(&client, tool, args).await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_handle.abort();

    let caps = captured.lock().unwrap().clone();
    (result, caps)
}

#[tokio::test]
async fn create_paths_return_bridge_error() {
    for (tool, args) in [
        ("daruma_create", json!({"task": {"title": "x"}})),
        ("daruma_capture", json!({"text": "x"})),
        ("daruma_capture_batch", json!({"texts": ["x"]})),
    ] {
        let (result, captured) = with_recording_server(tool, args).await;
        let err = result.expect_err(&format!("{tool} must be bridged"));
        let msg = err.to_string();
        assert!(msg.contains("plan_only_intake"), "{tool}: {msg}");
        assert!(msg.contains("daruma_plan_materialize"), "{tool}: {msg}");
        assert!(
            captured.is_empty(),
            "{tool} must not reach the server, got {captured:?}"
        );
    }
}

#[tokio::test]
async fn plan_materialize_posts_materialize_plan_command() {
    let (result, captured) = with_recording_server(
        "daruma_plan_materialize",
        json!({
            "plan": {"title": "Wave 1", "project_id": "prj-1", "goal": "ship"},
            "tasks": [
                {"title": "step 1"},
                {"title": "step 2", "priority": "p1"},
            ],
        }),
    )
    .await;
    result.expect("materialize must succeed against 200 {}");

    let cap = captured
        .iter()
        .find(|c| c.path == "/v1/commands")
        .expect("materialize must POST /v1/commands");
    let command = &cap.body["command"];
    assert_eq!(command["type"], "materialize_plan", "{command}");
    assert_eq!(command["plan"]["title"], "Wave 1");
    assert_eq!(command["plan"]["project_id"], "prj-1");
    assert_eq!(command["plan"]["goal"], "ship");
    assert_eq!(command["plan"]["owner"]["kind"], "user");
    let tasks = command["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0]["title"], "step 1");
    assert_eq!(tasks[1]["priority"], "p1");
}

#[tokio::test]
async fn plan_materialize_forwards_source_brief() {
    let (result, captured) = with_recording_server(
        "daruma_plan_materialize",
        json!({
            "plan": {
                "title": "From brief",
                "project_id": "prj-1",
                "source_brief": "raw prompt"
            },
            "tasks": [{"title": "step 1"}],
        }),
    )
    .await;
    result.expect("materialize must succeed against 200 {}");

    let cap = captured
        .iter()
        .find(|c| c.path == "/v1/commands")
        .expect("materialize must POST /v1/commands");
    let body = &cap.body["command"];
    assert_eq!(body["plan"]["source_brief"], "raw prompt");
}

#[tokio::test]
async fn plan_materialize_requires_tasks() {
    for args in [
        json!({"plan": {"title": "no tasks", "project_id": "prj-1"}}),
        json!({"plan": {"title": "empty", "project_id": "prj-1"}, "tasks": []}),
    ] {
        let (result, captured) = with_recording_server("daruma_plan_materialize", args).await;
        let err = result.expect_err("tasks are required");
        assert!(err.to_string().contains("tasks"), "{err}");
        assert!(captured.is_empty(), "no request expected, got {captured:?}");
    }
}

/// ADR-0009: `plan.source` (the chain's nearest node) and
/// `plan.git_context` pass through; wrong shapes fail before any request.
#[tokio::test]
async fn plan_materialize_maps_source_and_git_context() {
    let (result, captured) = with_recording_server(
        "daruma_plan_materialize",
        json!({
            "plan": {
                "title": "Fix login",
                "project_id": "prj_1",
                "source": {"ref": "https://gitlab.x/g/p/-/issues/7"},
                "source_brief": "from standup",
                "git_context": {"branch": "7_fix_login"}
            },
            "tasks": [{"title": "t"}]
        }),
    )
    .await;
    result.unwrap();
    let plan = &captured[0].body["command"]["plan"];
    assert_eq!(plan["source"]["ref"], "https://gitlab.x/g/p/-/issues/7");
    assert_eq!(plan["source_brief"], "from standup");
    assert_eq!(plan["git_context"]["branch"], "7_fix_login");

    for (bad, needle) in [
        (json!({"source": "https://x"}), "`source` must be an object"),
        (
            json!({"git_context": "main"}),
            "`git_context` must be an object",
        ),
    ] {
        let mut plan = json!({"title": "x", "project_id": "prj_1"});
        for (k, v) in bad.as_object().unwrap() {
            plan[k] = v.clone();
        }
        let (result, captured) = with_recording_server(
            "daruma_plan_materialize",
            json!({"plan": plan, "tasks": [{"title": "t"}]}),
        )
        .await;
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains(needle), "{msg}");
        assert!(captured.is_empty());
    }
}

/// `intake_source` reaches PATCH /settings as given, and `null` is kept (it
/// removes the policy server-side).
#[tokio::test]
async fn project_settings_update_forwards_intake_source() {
    let policy = json!({"mode": "enforce", "channels": [{"scheme": "https"}]});
    let (result, captured) = with_recording_server(
        "daruma_project_settings_update",
        json!({"project_id": "prj_1", "intake_source": policy}),
    )
    .await;
    result.unwrap();
    assert_eq!(captured[0].path, "/v1/projects/prj_1/settings");
    assert_eq!(captured[0].body["intake_source"], policy);

    let (result, captured) = with_recording_server(
        "daruma_project_settings_update",
        json!({"project_id": "prj_1", "intake_source": null}),
    )
    .await;
    result.unwrap();
    assert!(captured[0].body["intake_source"].is_null());
    assert!(captured[0]
        .body
        .as_object()
        .unwrap()
        .contains_key("intake_source"));
}

/// `daruma_plan_create` takes the same `source`/`git_context` arguments as
/// materialize and forwards them to `POST /v1/plans`.
#[tokio::test]
async fn plan_create_maps_source_and_git_context() {
    let (result, captured) = with_recording_server(
        "daruma_plan_create",
        json!({
            "title": "Plan",
            "project_id": "prj_1",
            "source": {"ref": "mailto:c@acme.ru", "note": "n", "upstream": [{"label": "Call"}]},
            "source_brief": "asked by mail",
            "git_context": {"branch": "12_x"}
        }),
    )
    .await;
    result.unwrap();
    assert_eq!(captured[0].path, "/v1/plans");
    let plan = &captured[0].body["plan"];
    assert_eq!(plan["source"]["ref"], "mailto:c@acme.ru");
    assert_eq!(plan["source"]["note"], "n");
    assert_eq!(plan["source"]["upstream"][0]["label"], "Call");
    assert_eq!(plan["source_brief"], "asked by mail");
    assert_eq!(plan["git_context"]["branch"], "12_x");
}

/// `daruma_source_extend` forwards its arguments to `POST /v1/sources/extend`.
#[tokio::test]
async fn source_extend_forwards_arguments() {
    let (result, captured) = with_recording_server(
        "daruma_source_extend",
        json!({"ref": "mailto:c@acme.ru", "upstream": [{"label": "Call", "occurred_at": "2026-01-01"}]}),
    )
    .await;
    result.unwrap();
    assert_eq!(captured[0].path, "/v1/sources/extend");
    assert_eq!(captured[0].body["ref"], "mailto:c@acme.ru");
    assert_eq!(captured[0].body["upstream"][0]["label"], "Call");
    assert!(captured[0].body.get("plan_id").is_none());
}
