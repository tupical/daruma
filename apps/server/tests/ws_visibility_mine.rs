//! `/v1/ws?visibility=mine` — the live feed keeps only events the principal
//! authored or about its own tasks (created, completed, or claimed through
//! an `agent_claimed` event it wrote). The principal is the embedding host's
//! `HostPrincipal`, else the token's agent id. Without the parameter the feed
//! is unchanged; an unknown value fails the upgrade with 400.

use std::net::SocketAddr;
use std::time::Duration;

use axum::Extension;
use chrono::Utc;
use daruma_auth::HostPrincipal;
use daruma_domain::{Actor, NewTask, Status};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{AgentId, ClaimId, ProjectId, TaskId};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Error as WsError, Message},
};

mod common;
use common::{test_app, TestApp};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsSink = futures::stream::SplitSink<Ws, Message>;
type WsStream = futures::stream::SplitStream<Ws>;

/// Serve the app, injecting `principal` as the host would (cloud gateway).
async fn spawn_with_principal(app: &TestApp, principal: Option<AgentId>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = match principal {
        Some(id) => app.router.clone().layer(Extension(HostPrincipal(id))),
        None => app.router.clone(),
    };
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

async fn subscribe(addr: SocketAddr, token: &str, query: &str) -> (WsSink, WsStream) {
    let mut req = format!("ws://{addr}/v1/ws{query}")
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        format!("daruma.v1, bearer.{token}").parse().unwrap(),
    );
    let (ws, _) = connect_async(req).await.expect("WS connect");
    let (mut sink, mut stream) = ws.split();
    let _ = next_json(&mut stream, Duration::from_secs(2)).await; // Hello
    sink.send(Message::Text(
        json!({ "type": "subscribe" }).to_string().into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    (sink, stream)
}

async fn next_json(stream: &mut WsStream, timeout: Duration) -> Option<Value> {
    let msg = tokio::time::timeout(timeout, stream.next())
        .await
        .ok()??
        .ok()?;
    match msg {
        Message::Text(t) => serde_json::from_str(&t).ok(),
        _ => None,
    }
}

/// Task ids (bare uuid) of every `event` frame within `timeout`.
async fn event_tasks(stream: &mut WsStream, timeout: Duration) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return seen;
        }
        let Some(frame) = next_json(stream, remaining).await else {
            return seen;
        };
        if frame["type"] != "event" {
            continue;
        }
        let payload = &frame["envelope"]["payload"];
        let id = payload["task_id"]
            .as_str()
            .or_else(|| payload["task"]["id"].as_str())
            .unwrap_or("-");
        seen.push(id.to_string());
    }
}

async fn seed_task(app: &TestApp, author: Actor) -> TaskId {
    let task_id = TaskId::new();
    let mut nt = NewTask::new("seeded");
    nt.id = Some(task_id);
    nt.project_id = Some(ProjectId::new());
    let env = EventEnvelope::new(author, Event::TaskCreated { task: nt });
    app.state.tasks.apply_event(&env).await.unwrap();
    task_id
}

fn status_changed(task_id: TaskId, seq: u64, actor: Actor) -> EventEnvelope {
    EventEnvelope {
        seq,
        ..EventEnvelope::new(
            actor,
            Event::TaskStatusChanged {
                task_id,
                from: Status::Todo,
                to: Status::InProgress,
            },
        )
    }
}

#[tokio::test]
async fn mine_keeps_own_tasks_and_own_events_only() {
    let app = test_app().await;
    let me = AgentId::new();
    let other = AgentId::new();
    let addr = spawn_with_principal(&app, Some(me)).await;

    let created = seed_task(&app, Actor::user_with_id(me)).await;
    let foreign = seed_task(&app, Actor::user_with_id(other)).await;

    // A foreign task becomes mine through a live claim whose
    // `agent_claimed` event I authored — the claim row itself is keyed by
    // some token agent id, as for cloud tokens.
    let claimed = seed_task(&app, Actor::user_with_id(other)).await;
    let claim_id = ClaimId::new();
    let token_agent = AgentId::new();
    let expires_at = Utc::now() + chrono::Duration::seconds(300);
    app.state
        .claims
        .acquire_until(token_agent, claimed, claim_id, expires_at)
        .await
        .unwrap();
    app.state
        .store
        .append(EventEnvelope::new(
            Actor::user_with_id(me),
            Event::AgentClaimed {
                agent_id: token_agent,
                task_id: claimed,
                claim_id: Some(claim_id),
                expires_at,
            },
        ))
        .await
        .unwrap();

    let (_mine_sink, mut mine) = subscribe(addr, &app.admin_token, "?visibility=mine").await;
    let (_all_sink, mut all) = subscribe(addr, &app.admin_token, "").await;

    let base = 1_000;
    app.bus.publish(status_changed(
        foreign,
        base + 1,
        Actor::user_with_id(other),
    ));
    app.bus.publish(status_changed(
        created,
        base + 2,
        Actor::user_with_id(other),
    ));
    app.bus.publish(status_changed(
        claimed,
        base + 3,
        Actor::user_with_id(other),
    ));
    // My own write on someone else's task is mine (journal rule F).
    app.bus
        .publish(status_changed(foreign, base + 4, Actor::user_with_id(me)));

    let bare = |t: TaskId| t.as_uuid().to_string();
    assert_eq!(
        event_tasks(&mut mine, Duration::from_millis(800)).await,
        vec![bare(created), bare(claimed), bare(foreign)],
        "mine: own-created, own-claimed, own-authored only"
    );
    assert_eq!(
        event_tasks(&mut all, Duration::from_millis(400))
            .await
            .len(),
        4,
        "no visibility parameter keeps the full feed"
    );
}

#[tokio::test]
async fn mine_without_host_uses_token_principal() {
    let app = test_app().await;
    let addr = spawn_with_principal(&app, None).await;
    let other = AgentId::new();

    let own = seed_task(&app, Actor::user_with_id(app.admin_agent_id)).await;
    let foreign = seed_task(&app, Actor::user_with_id(other)).await;

    let (_mine_sink, mut mine) = subscribe(addr, &app.admin_token, "?visibility=mine").await;
    app.bus
        .publish(status_changed(foreign, 1_001, Actor::user_with_id(other)));
    app.bus
        .publish(status_changed(own, 1_002, Actor::user_with_id(other)));

    assert_eq!(
        event_tasks(&mut mine, Duration::from_millis(600)).await,
        vec![own.as_uuid().to_string()]
    );
}

#[tokio::test]
async fn unknown_visibility_is_rejected_before_upgrade() {
    let app = test_app().await;
    let addr = spawn_with_principal(&app, None).await;
    let mut req = format!("ws://{addr}/v1/ws?visibility=bogus")
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        format!("daruma.v1, bearer.{}", app.admin_token)
            .parse()
            .unwrap(),
    );
    match connect_async(req).await {
        Err(WsError::Http(resp)) => assert_eq!(resp.status(), 400),
        other => panic!("expected HTTP 400, got {other:?}"),
    }
}
