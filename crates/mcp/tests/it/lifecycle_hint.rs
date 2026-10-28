//! Declarative lifecycle hint on task mutations (daruma 01a17eb7-…-ba8af15694b8).
//!
//! Every task mutation answers with where the task is now (`task {id,
//! status, claim_expires_at?}`) and which lifecycle step follows
//! (`next_tool {name, arguments}` or `null`). `initialize.instructions` names
//! the agent's open tasks. Snapshot-style: the exact hint shape is asserted.
//!
//! Stub server answers like the real HTTP API for the routes involved; the
//! task status it reports is per-test so the post-mutation read-back is
//! observable.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{body::Body, extract::Request, http::StatusCode, routing::any, Router};
use daruma_mcp::tools::call_tool;
use daruma_mcp::{
    dispatch_request_with_profile, ApiClient, JsonRpcRequest, ToolProfile, OPEN_TASKS_MARKER,
    OPEN_TASKS_META_KEY,
};
use serde_json::{json, Value};

const AGENT: &str = "01a12009-595d-7251-a3fd-fb84fe9c4518";
const TASK: &str = "01a17eb7-203a-7f11-b8b2-ba8af15694b8";
const OTHER: &str = "01a17eb7-203a-7f11-b8b2-ba7124a8d402";
const PLAN: &str = "01a17eb7-2038-7b72-b602-f7d21cbe23ad";

#[derive(Clone)]
struct Stub {
    /// Status `GET /v1/tasks/{TASK}` reports; `None` makes the read fail.
    status: Option<&'static str>,
    requests: Arc<Mutex<Vec<String>>>,
}

fn respond(stub: &Stub, method: &str, path: &str, body: &Value) -> (StatusCode, Value) {
    stub.requests
        .lock()
        .unwrap()
        .push(format!("{method} {path}"));
    match (method, path) {
        ("POST", "/v1/mcp") => {
            let info = json!({"mcp_agent_id": AGENT}).to_string();
            (
                StatusCode::OK,
                json!({"result": {"content": [{"type": "text", "text": info}]}}),
            )
        }
        ("GET", "/v1/claims") => (
            StatusCode::OK,
            json!({"claims": [
                {"agent_id": AGENT, "task_id": TASK},
                {"agent_id": "01a00000-0000-7000-8000-000000000000", "task_id": OTHER},
                {"agent_id": AGENT, "task_id": OTHER},
            ]}),
        ),
        ("GET", p) if p == format!("/v1/tasks/{TASK}") => match stub.status {
            Some(status) => (
                StatusCode::OK,
                json!({"id": TASK, "title": "T3 Результат мутации ведёт дальше", "status": status,
                       "current_claim": {"agent_id": AGENT, "is_mine": true,
                                         "expires_at": "2026-10-28T14:05:55Z"}}),
            ),
            None => (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "down"})),
        },
        // Someone else's claim on a closed task: never an open task of ours.
        ("GET", p) if p == format!("/v1/tasks/{OTHER}") => (
            StatusCode::OK,
            json!({"id": OTHER, "title": "closed", "status": "done", "current_claim": null}),
        ),
        ("POST", "/v1/claims") => (
            StatusCode::OK,
            json!({"success": true, "event_id": "evt_1", "event_seq": 5,
                   "data": {"agent_id": AGENT, "task_id": body["task_id"].clone()}}),
        ),
        ("POST", p) if p.ends_with("/drain-next") => (
            StatusCode::OK,
            json!({"plan_id": PLAN, "task_id": TASK, "run_id": "01a18267-1387-7771-9551-120f699a3794",
                   "claim_expires_at": "2026-10-28T14:05:55Z", "position": 3}),
        ),
        ("POST", p) if p.ends_with("/comments") => (
            StatusCode::OK,
            json!({"id": "01a18267-4964-7861-84fe-47b5805156a9", "task_id": TASK,
                   "kind": body["kind"].clone(), "created_at": "2026-10-28T12:06:09Z"}),
        ),
        ("POST", "/v1/commands") => {
            let command = &body["command"];
            match command["type"].as_str() {
                Some("materialize_plan") => {
                    let mut events = vec![json!({"id": "evt_1", "seq": 10, "payload":
                        {"type": "plan_created", "plan": {"id": PLAN, "status": "draft"}}})];
                    for (ix, task) in [TASK, OTHER].iter().enumerate() {
                        events.push(json!({"id": format!("evt_c{ix}"), "seq": 11, "payload":
                            {"type": "task_created", "task": {"id": task, "status": "todo"}}}));
                        events.push(json!({"id": format!("evt_a{ix}"), "seq": 12, "payload":
                            {"type": "plan_task_added", "plan_id": PLAN, "task_id": task,
                             "position": ix}}));
                    }
                    (StatusCode::OK, json!({"success": true, "data": events}))
                }
                Some("complete_task") => (
                    StatusCode::OK,
                    json!({"success": true, "event_id": "evt_9", "event_seq": 99, "data": [
                        {"id": "evt_9", "seq": 99, "payload": {"type": "task_status_changed",
                         "task_id": command["id"].clone(), "from": "in_review", "to": "done"}}]}),
                ),
                other => panic!("unexpected command {other:?}"),
            }
        }
        other => panic!("unexpected request {other:?}"),
    }
}

async fn serve(stub: Stub) -> (ApiClient, tokio::task::JoinHandle<()>) {
    serve_with_claims_delay(stub, Duration::ZERO).await
}

/// [`serve`] whose `GET /v1/claims` answers only after `delay`.
async fn serve_with_claims_delay(
    stub: Stub,
    delay: Duration,
) -> (ApiClient, tokio::task::JoinHandle<()>) {
    let router = Router::new().fallback(any(move |req: Request<Body>| {
        let stub = stub.clone();
        async move {
            let method = req.method().to_string();
            let path = req.uri().path().to_string();
            if method == "GET" && path == "/v1/claims" {
                tokio::time::sleep(delay).await;
            }
            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .unwrap_or_default();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let (status, json) = respond(&stub, &method, &path, &body);
            (status, axum::Json(json))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (
        ApiClient::new(format!("http://{addr}"), "test-token"),
        handle,
    )
}

async fn call(status: Option<&'static str>, tool: &str, args: Value) -> (Value, Vec<String>) {
    let stub = Stub {
        status,
        requests: Arc::default(),
    };
    let (client, handle) = serve(stub.clone()).await;
    let result = call_tool(&client, tool, args).await.expect("tool call");
    handle.abort();
    let requests = stub.requests.lock().unwrap().clone();
    (result, requests)
}

#[tokio::test]
async fn materialize_names_task_ids_and_the_drain_step() {
    let (result, _) = call(
        Some("todo"),
        "daruma_plan_materialize",
        json!({"plan": {"title": "Plan", "project_id": "019e5f3f-ab10-7451-8455-6f3807545eb9"},
               "tasks": [{"title": "a"}, {"title": "b"}]}),
    )
    .await;
    assert_eq!(result["task_ids"], json!([TASK, OTHER]));
    assert_eq!(
        result["next_tool"],
        json!({"name": "daruma_plan_drain_next", "arguments": {"plan_id": PLAN}})
    );
    assert!(result.get("task").is_none());
}

#[tokio::test]
async fn drain_reports_the_claimed_task_and_the_outcome_step() {
    let (result, requests) = call(
        Some("in_progress"),
        "daruma_plan_drain_next",
        json!({"plan_id": PLAN, "claim_ttl_secs": 7200}),
    )
    .await;
    assert_eq!(
        result["task"],
        json!({"id": TASK, "status": "in_progress", "claim_expires_at": "2026-10-28T14:05:55Z"})
    );
    assert_eq!(
        result["next_tool"],
        json!({"name": "daruma_comment", "arguments": {"task_id": TASK, "kind": "outcome"}})
    );
    // The drain result itself is untouched.
    assert_eq!(result["run_id"], "01a18267-1387-7771-9551-120f699a3794");
    assert!(requests.contains(&format!("GET /v1/tasks/{TASK}")));
}

#[tokio::test]
async fn claim_reports_the_task_and_the_outcome_step() {
    let (result, _) = call(
        Some("in_progress"),
        "daruma_claim",
        json!({"task_id": TASK}),
    )
    .await;
    assert_eq!(result["task"]["status"], "in_progress");
    assert_eq!(result["next_tool"]["name"], "daruma_comment");
    assert_eq!(result["next_tool"]["arguments"]["task_id"], TASK);
}

#[tokio::test]
async fn outcome_comment_points_at_complete_while_the_task_is_open() {
    let (result, _) = call(
        Some("in_review"),
        "daruma_comment",
        json!({"task_id": TASK, "kind": "outcome", "body": "done and verified"}),
    )
    .await;
    assert_eq!(
        result["task"],
        json!({"id": TASK, "status": "in_review", "claim_expires_at": "2026-10-28T14:05:55Z"})
    );
    assert_eq!(
        result["next_tool"],
        json!({"name": "daruma_complete", "arguments": {"id": TASK}})
    );
}

#[tokio::test]
async fn other_comment_kinds_carry_no_hint_and_no_extra_read() {
    let (result, requests) = call(
        Some("in_progress"),
        "daruma_comment",
        json!({"task_id": TASK, "kind": "progress", "body": "step two"}),
    )
    .await;
    assert!(result.get("next_tool").is_none(), "{result}");
    assert!(result.get("task").is_none(), "{result}");
    assert_eq!(requests, vec![format!("POST /v1/tasks/{TASK}/comments")]);
}

#[tokio::test]
async fn complete_ends_the_lifecycle() {
    let (result, _) = call(Some("done"), "daruma_complete", json!({"id": TASK})).await;
    assert_eq!(result["task"]["status"], "done");
    assert_eq!(result["next_tool"], Value::Null);
    assert_eq!(result["success"], true);
}

#[tokio::test]
async fn unreadable_task_keeps_the_mutation_result() {
    let (result, _) = call(
        None,
        "daruma_comment",
        json!({"task_id": TASK, "kind": "outcome", "body": "done and verified"}),
    )
    .await;
    assert_eq!(result["id"], "01a18267-4964-7861-84fe-47b5805156a9");
    assert!(result.get("task").is_none());
    // No task state, no guess about the next step.
    assert_eq!(result["next_tool"], Value::Null);
}

fn initialize() -> JsonRpcRequest {
    serde_json::from_value(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"})).unwrap()
}

#[tokio::test]
async fn initialize_names_this_agents_open_tasks() {
    let stub = Stub {
        status: Some("in_progress"),
        requests: Arc::default(),
    };
    let (client, handle) = serve(stub).await;
    let response = dispatch_request_with_profile(&client, ToolProfile::Default, initialize())
        .await
        .unwrap();
    handle.abort();
    let result = response.result.unwrap();
    let text = result["instructions"].as_str().unwrap();
    assert!(text.chars().count() <= daruma_mcp::MAX_INSTRUCTIONS_CHARS);
    assert!(text.contains("outcome comment → in_review; daruma_complete → done"));
    let line = text.rsplit("\n\n").next().unwrap();
    assert_eq!(
        line,
        format!("{OPEN_TASKS_MARKER}{TASK} — \"T3 Результат мутации ведёт дальше\" — in_progress")
    );
    // The done task under this agent's claim and the foreign claim stay out.
    assert!(!text.contains(OTHER));
    assert_eq!(
        result["_meta"][OPEN_TASKS_META_KEY],
        json!([{"id": TASK, "title": "T3 Результат мутации ведёт дальше", "status": "in_progress"}])
    );
}

#[tokio::test]
async fn initialize_without_a_server_keeps_the_static_rules() {
    let client = ApiClient::new("http://127.0.0.1:1", "test-token");
    let response = dispatch_request_with_profile(&client, ToolProfile::Default, initialize())
        .await
        .unwrap();
    let result = response.result.unwrap();
    let text = result["instructions"].as_str().unwrap();
    assert!(!text.contains(OPEN_TASKS_MARKER));
    assert!(result.get("_meta").is_none());
}

#[tokio::test]
async fn initialize_past_the_lookup_ceiling_keeps_the_static_rules() {
    let stub = Stub {
        status: Some("in_progress"),
        requests: Arc::default(),
    };
    let late = daruma_mcp::OPEN_TASKS_LOOKUP_TIMEOUT + Duration::from_secs(1);
    let (client, handle) = serve_with_claims_delay(stub, late).await;
    let started = Instant::now();
    let response = dispatch_request_with_profile(&client, ToolProfile::Default, initialize())
        .await
        .unwrap();
    let elapsed = started.elapsed();
    handle.abort();
    let result = response.result.expect("initialize succeeds");
    let text = result["instructions"].as_str().unwrap();
    assert!(text.contains("outcome comment → in_review; daruma_complete → done"));
    assert!(!text.contains(OPEN_TASKS_MARKER));
    assert!(result.get("_meta").is_none());
    // Cut off at the ceiling: it waited on the slow read, but not for it.
    assert!(
        elapsed >= daruma_mcp::OPEN_TASKS_LOOKUP_TIMEOUT && elapsed < late,
        "initialize waited {elapsed:?}"
    );
}
