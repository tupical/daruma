//! Replica event ingress tests for desktop reconnect flush.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use daruma_domain::{Actor, NewTask};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{time, AgentId, ClaimId, DeviceId, TaskId};
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
async fn replica_claim_event_preserves_generation_in_event_and_projection() {
    let h = test_app().await;
    let agent_id = AgentId::new();
    let task_id = TaskId::new();
    let claim_id = ClaimId::new();
    let envelope = EventEnvelope::new(
        Actor::user(),
        Event::AgentClaimed {
            agent_id,
            task_id,
            claim_id: Some(claim_id),
            expires_at: time::now() + chrono::Duration::seconds(60),
        },
    );

    let (status, response) = post_json(
        &h.router,
        &h.admin_token,
        "/v1/events/replica",
        json!({ "events": [envelope] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");

    let claims = h.state.claims.list_active(None).await.unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].claim_id, claim_id);
    let events = h.state.store.load_since(0, 10).await.unwrap();
    assert!(matches!(
        events.as_slice(),
        [EventEnvelope {
            payload: Event::AgentClaimed {
                claim_id: Some(event_claim_id),
                ..
            },
            ..
        }] if *event_claim_id == claim_id
    ));
}
