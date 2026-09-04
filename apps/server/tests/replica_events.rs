//! Replica event ingress tests for desktop reconnect flush.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use daruma_domain::{Actor, NewTask, Run, RunStatus};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{time, AgentId, ClaimId, DeviceId, PlanId, RunId, TaskId};
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::test_app;

async fn post_json(app: &axum::Router, token: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn replica_event_replay_is_idempotent_and_updates_projection() {
    let h = test_app().await;
    let device = DeviceId::new();
    let mut envelope = EventEnvelope::new(
        Actor::user(),
        Event::TaskCreated {
            task: NewTask::new("offline replica task"),
        },
    );
    envelope.origin_device_id = Some(device);
    envelope.origin_seq = 1;
    let body = json!({ "events": [envelope] });

    let (status, first) = post_json(
        &h.router,
        &h.admin_token,
        "/v1/events/replica",
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["data"]["duplicates"], 0);

    let (status, second) = post_json(&h.router, &h.admin_token, "/v1/events/replica", body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["data"]["duplicates"], 1);

    let events = h.state.store.load_since(0, 100).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].origin_device_id, Some(device));
    assert_eq!(events[0].origin_seq, 1);
    assert_eq!(h.state.tasks.list_all().await.unwrap().len(), 1);
}

#[tokio::test]
async fn replica_rejects_run_claim_and_terminal_injection_before_persisting_any_event() {
    let h = test_app().await;
    let agent_id = AgentId::new();
    let task_id = TaskId::new();
    let plan_id = PlanId::new();
    let run_id = RunId::new();
    let now = time::now();
    let run = Run {
        id: run_id,
        plan_id,
        agent_id,
        parent_run_id: None,
        started_at: now,
        ended_at: None,
        status: RunStatus::Active,
        outcome: None,
        last_activity_at: None,
        unresponsive_at: None,
        stale_at: None,
    };
    let guarded = [
        Event::RunStarted { run },
        Event::AgentClaimed {
            agent_id,
            task_id,
            claim_id: Some(ClaimId::new()),
            expires_at: now + chrono::Duration::minutes(5),
        },
        Event::RunFailed {
            run_id,
            reason: "replica bypass".to_string(),
            at: now,
        },
        Event::PlanArchived { plan_id, at: now },
    ];

    for event in guarded {
        let mut envelope = EventEnvelope::new(Actor::user(), event);
        envelope.origin_device_id = Some(DeviceId::new());
        envelope.origin_seq = 1;
        let (status, response) = post_json(
            &h.router,
            &h.admin_token,
            "/v1/events/replica",
            json!({"events": [envelope]}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "replica bypass accepted: {response}"
        );
    }

    assert!(h.state.store.load_since(0, 100).await.unwrap().is_empty());
}
