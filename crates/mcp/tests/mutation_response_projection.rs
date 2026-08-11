//! Mutation-response projection on the MCP surface (token economy).
//!
//! The HTTP API echoes the request body back in mutation responses; the MCP
//! funnel used to forward that verbatim, so the agent paid tokens twice for
//! the same text. `call_tool` now projects mutating responses: values that
//! deep-equal something the client sent in the same call are dropped, result
//! fields (ids, status, seq, timestamps, errors) are always kept, and
//! `verbose: true` in the arguments returns the raw body.
//!
//! Verifies:
//!   1. `comment_response_drops_echoed_body_keeps_ids_and_seq` — the comment
//!      body text is gone; ids/seq/timestamps stay.
//!   2. `plan_materialize_keeps_created_ids_drops_echoed_titles` — created
//!      plan/task ids survive; echoed titles/goal do not.
//!   3. `plan_drain_next_result_is_not_projected_away` — the issued task and
//!      claim parameters are a result, not echo: nothing is lost.
//!   4. `status_change_event_keeps_from_and_to` — an event payload's
//!      `from`/`to` transition record survives even when `to` equals the
//!      status the caller sent (short enum-like strings are never echo).
//!   5. `verbose_true_returns_the_full_body` — the escape hatch works.
//!   6. `comment_response_size_shrinks_below_echo` — the actual byte measure:
//!      a ~4 KiB comment body stops dominating the response.

use axum::{body::Body, extract::Request, http::StatusCode, routing::any, Router};
use daruma_mcp::tools::call_tool;
use daruma_mcp::ApiClient;
use serde_json::{json, Value};

/// Full-echo stub: answers like the real HTTP API, mirroring request fields
/// back into the mutation response the way the server does.
fn stub_response(path: &str, body: &Value) -> Value {
    if path.ends_with("/comments") {
        let mut data = json!({
            "id": "cmt_1",
            "task_id": "tsk_1",
            "body": body["body"].clone(),
            "seq": 42,
            "created_at": "2026-08-11T00:00:00Z"
        });
        if let Some(kind) = body.get("kind") {
            data["kind"] = kind.clone();
        }
        return json!({ "success": true, "data": data });
    }
    if path == "/v1/commands" {
        let command = &body["command"];
        if command["type"] == "set_status" {
            // The server records the transition as an event: `from`/`to` are
            // the fact of what happened, produced server-side.
            return json!({
                "success": true,
                "data": [{
                    "id": "evt_9",
                    "seq": 77,
                    "recorded_at": "2026-08-11T00:00:00Z",
                    "payload": {
                        "type": "task_status_changed",
                        "task_id": command["id"].clone(),
                        "from": "todo",
                        "to": command["status"].clone()
                    }
                }]
            });
        }
        assert_eq!(command["type"], "materialize_plan");
        let plan = &command["plan"];
        let tasks = command["tasks"].as_array().expect("tasks array");
        let mut events = vec![json!({
            "id": "evt_1",
            "seq": 10,
            "recorded_at": "2026-08-11T00:00:00Z",
            "payload": {
                "type": "plan_created",
                "plan": {
                    "id": "pln_1",
                    "title": plan["title"].clone(),
                    "goal": plan["goal"].clone(),
                    "project_id": plan["project_id"].clone(),
                    "status": "draft",
                    "created_at": "2026-08-11T00:00:00Z"
                }
            }
        })];
        for (ix, task) in tasks.iter().enumerate() {
            events.push(json!({
                "id": format!("evt_{}", ix + 2),
                "seq": 11 + ix as u64,
                "recorded_at": "2026-08-11T00:00:00Z",
                "payload": {
                    "type": "task_created",
                    "task": {
                        "id": format!("tsk_{}", ix + 1),
                        "title": task["title"].clone(),
                        "status": "todo",
                        "plan_id": "pln_1",
                        "created_at": "2026-08-11T00:00:00Z"
                    }
                }
            }));
        }
        return json!({ "success": true, "data": events });
    }
    if path.ends_with("/drain-next") {
        // The issued task and the claim parameters are the *result* of the
        // call; the client cannot derive any of this from its arguments.
        return json!({
            "success": true,
            "data": {
                "task": {
                    "id": "tsk_9",
                    "title": "Implement the frobnicator",
                    "status": "in_progress",
                    "priority": "p1",
                    "plan_id": "pln_1",
                    "updated_at": "2026-08-11T00:00:00Z"
                },
                "run_id": "run_1",
                "claim_expires_at": "2026-08-11T01:00:00Z"
            }
        });
    }
    json!({})
}

async fn with_stub_server(tool: &str, args: Value) -> anyhow::Result<Value> {
    use tokio::net::TcpListener;

    let router = Router::new().fallback(any(move |req: Request<Body>| async move {
        let path = req.uri().path().to_string();
        let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (StatusCode::OK, axum::Json(stub_response(&path, &body)))
    }));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let client = ApiClient::new(format!("http://{addr}"), "test-token");
    let result = call_tool(&client, tool, args).await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_handle.abort();

    result
}

fn comment_args(body: &str) -> Value {
    json!({"task_id": "tsk_1", "body": body, "kind": "progress"})
}

#[tokio::test]
async fn comment_response_drops_echoed_body_keeps_ids_and_seq() {
    let body_text = "a unique comment body the client already knows";
    let result = with_stub_server("daruma_comment", comment_args(body_text))
        .await
        .expect("comment must succeed");
    let text = serde_json::to_string(&result).unwrap();

    assert!(!text.contains(body_text), "echoed body must be gone: {text}");

    let data = &result["data"];
    // `kind` was sent by the client too, but at 8 bytes it is below
    // ECHO_MIN_STRIP_LEN: short enum-like strings are kept by design — the
    // savings are noise, and stripping them is what sliced `to` out of
    // `task_status_changed` events.
    assert_eq!(
        data["kind"], "progress",
        "short enum-like echo is kept by design: {result}"
    );
    assert_eq!(data["id"], "cmt_1", "created comment id must stay: {result}");
    assert_eq!(data["task_id"], "tsk_1", "task id must stay: {result}");
    assert_eq!(data["seq"], 42, "seq must stay: {result}");
    assert!(
        data["created_at"].is_string(),
        "timestamp must stay: {result}"
    );
    assert_eq!(result["success"], true, "success flag must stay: {result}");
}

#[tokio::test]
async fn plan_materialize_keeps_created_ids_drops_echoed_titles() {
    // Titles/goal are long-form prose here: only strings above
    // ECHO_MIN_STRIP_LEN are eligible for stripping.
    let result = with_stub_server(
        "daruma_plan_materialize",
        json!({
            "plan": {
                "title": "Wave 9 rollout of the new planner across the fleet",
                "goal": "ship the planner migration to every region with zero downtime",
                "project_id": "prj_1"
            },
            "tasks": [
                {"title": "step one: migrate the alpha cohort to the new planner"},
                {"title": "step two: migrate the beta cohort to the new planner"}
            ],
        }),
    )
    .await
    .expect("materialize must succeed");
    let text = serde_json::to_string(&result).unwrap();

    for echoed in [
        "Wave 9 rollout of the new planner across the fleet",
        "ship the planner migration to every region with zero downtime",
        "step one: migrate the alpha cohort to the new planner",
        "step two: migrate the beta cohort to the new planner",
    ] {
        assert!(!text.contains(echoed), "echoed `{echoed}` must be gone: {text}");
    }
    // The created ids are the whole point of the response.
    assert!(text.contains("pln_1"), "plan id must stay: {text}");
    assert!(text.contains("tsk_1"), "first task id must stay: {text}");
    assert!(text.contains("tsk_2"), "second task id must stay: {text}");
    // Event metadata is server-produced, not echo.
    assert!(text.contains("plan_created"), "event type must stay: {text}");
    assert!(text.contains("task_created"), "event type must stay: {text}");
    assert_eq!(result["success"], true, "success flag must stay: {result}");
}

#[tokio::test]
async fn plan_drain_next_result_is_not_projected_away() {
    let result = with_stub_server(
        "daruma_plan_drain_next",
        json!({"plan_id": "pln_1", "run_id": "run_1"}),
    )
    .await
    .expect("drain_next must succeed");

    let task = &result["data"]["task"];
    assert_eq!(task["id"], "tsk_9", "issued task id must stay: {result}");
    assert_eq!(
        task["title"], "Implement the frobnicator",
        "issued task title is a result, not echo: {result}"
    );
    assert_eq!(task["status"], "in_progress", "status must stay: {result}");
    assert_eq!(
        result["data"]["claim_expires_at"], "2026-08-11T01:00:00Z",
        "claim parameters must stay: {result}"
    );
    assert_eq!(
        result["data"]["run_id"], "run_1",
        "run_id is part of the granted claim: {result}"
    );
}

#[tokio::test]
async fn status_change_event_keeps_from_and_to() {
    // `daruma_set_status` sends the target status as an argument, and the
    // server reports the transition as `payload.from`/`payload.to`. Both
    // halves are the fact of what happened; dropping `to` because it equals
    // the sent `status` leaves a misleading half-record ("changed FROM todo
    // TO … nothing").
    let result = with_stub_server(
        "daruma_set_status",
        json!({"id": "tsk_1", "status": "in_progress"}),
    )
    .await
    .expect("set_status must succeed");

    let payload = &result["data"][0]["payload"];
    assert_eq!(
        payload["type"], "task_status_changed",
        "event type must stay: {result}"
    );
    assert_eq!(payload["from"], "todo", "transition origin must stay: {result}");
    assert_eq!(
        payload["to"], "in_progress",
        "transition target must survive projection even though it equals the sent status: {result}"
    );
}

#[tokio::test]
async fn verbose_true_returns_the_full_body() {
    let body_text = "a unique comment body the client already knows";
    let mut args = comment_args(body_text);
    args["verbose"] = json!(true);
    let result = with_stub_server("daruma_comment", args)
        .await
        .expect("comment must succeed");
    let text = serde_json::to_string(&result).unwrap();

    assert!(
        text.contains(body_text),
        "verbose must return the unprojected body: {text}"
    );
    assert!(text.contains("progress"), "verbose keeps echo: {text}");
}

#[tokio::test]
async fn comment_response_size_shrinks_below_echo() {
    let body_text = "x".repeat(4096);

    let mut verbose_args = comment_args(&body_text);
    verbose_args["verbose"] = json!(true);
    let full = with_stub_server("daruma_comment", verbose_args)
        .await
        .expect("verbose comment must succeed");
    let full_bytes = serde_json::to_string(&full).unwrap().len();

    let projected = with_stub_server("daruma_comment", comment_args(&body_text))
        .await
        .expect("comment must succeed");
    let projected_bytes = serde_json::to_string(&projected).unwrap().len();

    eprintln!(
        "daruma_comment result_bytes: verbose(full)={full_bytes} projected={projected_bytes}"
    );
    assert!(
        full_bytes > 4096,
        "full body must carry the 4 KiB echo, got {full_bytes}"
    );
    assert!(
        projected_bytes < 512,
        "projected response must shrink to hundreds of bytes, got {projected_bytes}"
    );
}
