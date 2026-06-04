//! AC-13 — Plans/Runs channel subscription with per-event capability gating.
//!
//! Tests:
//!   * `subscribe_plans_filters_by_project` — Plans channel respects project filter.
//!   * `subscribe_runs_streams_run_events` — Runs channel delivers RunStarted.
//!   * `no_subscribe_plans_capability_no_events` — token without `SubscribePlans`
//!     gets nothing on the Plans channel (events silently dropped).
//!   * `mixed_capability_partial_visibility` — `SubscribeTasks`-only token
//!     subscribes to both channels; receives task events, not plan events.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use taskagent_auth::{
    generate, Capabilities, Capability, NewTokenSpec, ProjectFilter, TokenKind, TokenScope,
    TokenStore,
};
use taskagent_domain::{Actor, NewTask, Plan, PlanPatch, PlanStatus, Run, RunStatus};
use taskagent_events::{Event, EventBus, EventEnvelope};
use taskagent_shared::{AgentId, PlanId, ProjectId, RunId};
use tokio_tungstenite::{connect_async, tungstenite::Message};

mod common;
use common::{spawn_server as spawn_test_server, test_app};

// ── Server scaffold ───────────────────────────────────────────────────────────

struct PlansServer {
    addr: SocketAddr,
    admin_token: String,
    auth_store: Arc<dyn TokenStore>,
    bus: EventBus,
}

async fn spawn_server() -> PlansServer {
    let app = test_app().await;
    let addr = spawn_test_server(&app).await;
    PlansServer {
        addr,
        admin_token: app.admin_token,
        auth_store: app.state.auth_store.clone(),
        bus: app.bus,
    }
}

// ── Helper types ──────────────────────────────────────────────────────────────

type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type WsStream = futures::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

// ── Helper functions ──────────────────────────────────────────────────────────

/// Create and persist a token with specific capabilities; return the plaintext.
async fn make_token(store: &Arc<dyn TokenStore>, capabilities: Capabilities) -> String {
    let secret = generate(NewTokenSpec {
        kind: TokenKind::Svc,
        agent_id: AgentId::new(),
        scope: TokenScope {
            projects: ProjectFilter::All,
            capabilities,
        },
        rate_limit_per_min: 300,
        expired_at: None,
    })
    .unwrap();
    store.insert(secret.record.clone()).await.unwrap();
    secret.plaintext
}

async fn connect_ws(addr: SocketAddr, token: &str) -> (WsSink, WsStream) {
    let url = format!("ws://{addr}/v1/ws?token={token}");
    let (stream, _) = connect_async(&url).await.expect("WS connect");
    stream.split()
}

async fn next_json(stream: &mut WsStream, timeout: Duration) -> Option<Value> {
    let next = tokio::time::timeout(timeout, stream.next()).await.ok()??;
    let msg = next.ok()?;
    let text = match msg {
        Message::Text(t) => t.to_string(),
        _ => return None,
    };
    serde_json::from_str(&text).ok()
}

/// Skip hello/ping/pong frames and return the first `"event"` frame, or `None` on timeout.
async fn drain_for_event(stream: &mut WsStream, timeout: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let frame = next_json(stream, remaining).await?;
        if frame.get("type").and_then(|v| v.as_str()) == Some("event") {
            return Some(frame);
        }
    }
}

/// Returns `true` if no `"event"` frame arrives within `timeout`.
async fn no_event_in(stream: &mut WsStream, timeout: Duration) -> bool {
    drain_for_event(stream, timeout).await.is_none()
}

/// Build a `PlanCreated` envelope with the given project and seq (must be > 0).
fn plan_created_env(project_id: ProjectId, seq: u64) -> EventEnvelope {
    use taskagent_shared::time;
    let now = time::now();
    let plan = Plan {
        id: PlanId::new(),
        project_id,
        parent_plan_id: None,
        title: "test plan".to_string(),
        description: String::new(),
        goal: String::new(),
        success_criteria: vec![],
        status: PlanStatus::Draft,
        owner: Actor::user(),
        created_at: now,
        updated_at: now,
        archived_at: None,
        source_brief: None,
    };
    EventEnvelope {
        seq,
        ..EventEnvelope::new(Actor::user(), Event::PlanCreated { plan })
    }
}

/// Build a `RunStarted` envelope with the given seq.
fn run_started_env(seq: u64) -> EventEnvelope {
    use taskagent_shared::time;
    let run = Run {
        id: RunId::new(),
        plan_id: PlanId::new(),
        agent_id: AgentId::new(),
        parent_run_id: None,
        started_at: time::now(),
        ended_at: None,
        status: RunStatus::Active,
        outcome: None,
        last_activity_at: None,
        unresponsive_at: None,
        stale_at: None,
    };
    EventEnvelope {
        seq,
        ..EventEnvelope::new(Actor::user(), Event::RunStarted { run })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// AC-13 part 1: `Channel::Plans` subscription respects the project filter.
/// Client A (project A) receives PlanCreated for project A.
/// Client B (project B) does NOT receive it.
#[tokio::test]
async fn subscribe_plans_filters_by_project() {
    let server = spawn_server().await;
    let project_a = ProjectId::new();
    let project_b = ProjectId::new();

    // Client A — subscribed to project A's Plans channel.
    let (mut sink_a, mut stream_a) = connect_ws(server.addr, &server.admin_token).await;
    let _ = next_json(&mut stream_a, Duration::from_secs(2)).await; // consume Hello
    sink_a
        .send(Message::Text(
            json!({
                "type": "subscribe",
                "projects": [project_a.as_uuid().to_string()],
                "channels": ["plans"]
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    // Client B — subscribed to project B's Plans channel.
    let (mut sink_b, mut stream_b) = connect_ws(server.addr, &server.admin_token).await;
    let _ = next_json(&mut stream_b, Duration::from_secs(2)).await;
    sink_b
        .send(Message::Text(
            json!({
                "type": "subscribe",
                "projects": [project_b.as_uuid().to_string()],
                "channels": ["plans"]
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(150)).await;

    // Publish PlanCreated for project A only.
    server.bus.publish(plan_created_env(project_a, 1));

    // Client A must receive it.
    assert!(
        drain_for_event(&mut stream_a, Duration::from_secs(2))
            .await
            .is_some(),
        "client A must receive PlanCreated for its project"
    );

    // Client B must not receive it.
    assert!(
        no_event_in(&mut stream_b, Duration::from_millis(400)).await,
        "client B must NOT receive PlanCreated for project A"
    );
}

/// Runs channel delivers `RunStarted` to subscribers.
#[tokio::test]
async fn subscribe_runs_streams_run_events() {
    let server = spawn_server().await;

    let (mut sink, mut stream) = connect_ws(server.addr, &server.admin_token).await;
    let _ = next_json(&mut stream, Duration::from_secs(2)).await;
    sink.send(Message::Text(
        json!({"type": "subscribe", "channels": ["runs"]})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    server.bus.publish(run_started_env(1));

    let got = drain_for_event(&mut stream, Duration::from_secs(2)).await;
    assert!(
        got.is_some(),
        "subscriber must receive RunStarted on runs channel"
    );
    let payload_type = got.unwrap()["envelope"]["payload"]["type"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    assert_eq!(payload_type, "run_started");
}

/// Token without `SubscribePlans` subscribes to Plans channel — events are
/// silently dropped.  Token with `SubscribePlans` receives them.
#[tokio::test]
async fn no_subscribe_plans_capability_no_events() {
    let server = spawn_server().await;

    // Token that passes the subscribe check (has SubscribeTasks) but lacks SubscribePlans.
    let token_no_plans = make_token(&server.auth_store, [Capability::SubscribeTasks].into()).await;
    // Token that has SubscribePlans.
    let token_yes_plans = make_token(&server.auth_store, [Capability::SubscribePlans].into()).await;

    // Connect both clients and subscribe to the Plans channel.
    let (mut sink_no, mut stream_no) = connect_ws(server.addr, &token_no_plans).await;
    let _ = next_json(&mut stream_no, Duration::from_secs(2)).await;
    sink_no
        .send(Message::Text(
            json!({"type": "subscribe", "channels": ["plans"]})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    let (mut sink_yes, mut stream_yes) = connect_ws(server.addr, &token_yes_plans).await;
    let _ = next_json(&mut stream_yes, Duration::from_secs(2)).await;
    sink_yes
        .send(Message::Text(
            json!({"type": "subscribe", "channels": ["plans"]})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(150)).await;

    server.bus.publish(plan_created_env(ProjectId::new(), 1));

    // Capable token must receive the event.
    assert!(
        drain_for_event(&mut stream_yes, Duration::from_secs(2))
            .await
            .is_some(),
        "token with SubscribePlans must receive PlanCreated"
    );

    // Incapable token must receive nothing.
    assert!(
        no_event_in(&mut stream_no, Duration::from_millis(400)).await,
        "token lacking SubscribePlans must NOT receive PlanCreated"
    );
}

/// W2: `PlanUpdated` with `parent_plan_id` diff is broadcast on `Channel::Plans`.
/// The subscriber receives an event whose patch contains the new parent id.
#[tokio::test]
async fn plan_reparent_emits_plan_updated_on_plans_channel() {
    let server = spawn_server().await;
    let parent_id = PlanId::new();
    let plan_id = PlanId::new();

    let (mut sink, mut stream) = connect_ws(server.addr, &server.admin_token).await;
    let _ = next_json(&mut stream, Duration::from_secs(2)).await; // consume Hello
    sink.send(Message::Text(
        json!({"type": "subscribe", "channels": ["plans"]})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(150)).await;

    // Publish PlanUpdated carrying a parent_plan_id change.
    let env = EventEnvelope {
        seq: 1,
        ..EventEnvelope::new(
            Actor::user(),
            Event::PlanUpdated {
                plan_id,
                patch: PlanPatch {
                    title: None,
                    description: None,
                    goal: None,
                    success_criteria: None,
                    parent_plan_id: Some(Some(parent_id)),
                },
            },
        )
    };
    server.bus.publish(env);

    let got = drain_for_event(&mut stream, Duration::from_secs(2)).await;
    assert!(
        got.is_some(),
        "Channel::Plans subscriber must receive PlanUpdated"
    );

    let frame = got.unwrap();
    let event_type = frame["envelope"]["payload"]["type"].as_str().unwrap_or("");
    assert_eq!(
        event_type, "plan_updated",
        "event type must be plan_updated"
    );

    // parent_plan_id must be visible in the serialised patch.
    let patch = &frame["envelope"]["payload"]["patch"];
    assert_eq!(
        patch["parent_plan_id"].as_str(),
        Some(parent_id.as_uuid().to_string().as_str()),
        "patch must carry the new parent_plan_id"
    );
}

/// Token with only `SubscribeTasks` subscribes to both channels.
/// Receives task events; plan events are silently dropped.
#[tokio::test]
async fn mixed_capability_partial_visibility() {
    let server = spawn_server().await;

    let token = make_token(&server.auth_store, [Capability::SubscribeTasks].into()).await;

    let (mut sink, mut stream) = connect_ws(server.addr, &token).await;
    let _ = next_json(&mut stream, Duration::from_secs(2)).await;
    sink.send(Message::Text(
        json!({"type": "subscribe", "channels": ["tasks", "plans"]})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(150)).await;

    // Publish PlanCreated first — should be silently dropped by capability gate.
    server.bus.publish(plan_created_env(ProjectId::new(), 1));

    // Then a TaskCreated — should flow through.
    let task_env = EventEnvelope {
        seq: 2,
        ..EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated {
                task: NewTask::new("mixed visibility"),
            },
        )
    };
    server.bus.publish(task_env);

    // First event received must be the task, not the plan.
    let got = drain_for_event(&mut stream, Duration::from_secs(2)).await;
    assert!(
        got.is_some(),
        "SubscribeTasks token must receive TaskCreated"
    );
    let payload_type = got.unwrap()["envelope"]["payload"]["type"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    assert_eq!(
        payload_type, "task_created",
        "must be task_created, not plan_created"
    );

    // No further event (PlanCreated was dropped).
    assert!(
        no_event_in(&mut stream, Duration::from_millis(400)).await,
        "no additional events expected after task_created (plan was dropped)"
    );
}
