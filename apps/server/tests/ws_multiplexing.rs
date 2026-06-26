//! AC-3 — `/v1/ws` per-project + per-channel multiplexing.
//!
//! Two WebSocket clients connect to a real listening server. Each
//! subscribes to a different project filter. We drive task creation
//! through the in-process `CommandBus` and assert that each client
//! receives **only** events for the projects it subscribed to.

use std::net::SocketAddr;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use daruma_core::{Command, CommandBus};
use daruma_domain::{Actor, NewTask};
use daruma_events::Event;
use daruma_shared::ProjectId;
use tokio_tungstenite::{connect_async, tungstenite::Message};

mod common;
use common::{spawn_server as spawn_test_server, test_app};

struct ServerHandle {
    addr: SocketAddr,
    token: String,
    commands: CommandBus,
}

async fn spawn_server() -> ServerHandle {
    let app = test_app().await;
    let addr = spawn_test_server(&app).await;
    ServerHandle {
        addr,
        token: app.admin_token,
        commands: app.state.commands,
    }
}

async fn connect_ws(
    addr: SocketAddr,
    token: &str,
) -> (
    futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
) {
    let url = format!("ws://{addr}/v1/ws?token={token}");
    let (stream, _resp) = connect_async(&url).await.expect("WS connect");
    stream.split()
}

async fn next_json(
    stream: &mut futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    timeout: Duration,
) -> Option<Value> {
    let next = tokio::time::timeout(timeout, stream.next()).await.ok()?;
    let msg = next?.ok()?;
    let text = match msg {
        Message::Text(t) => t.to_string(),
        _ => return None,
    };
    serde_json::from_str(&text).ok()
}

async fn drain_until_event(
    stream: &mut futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    expected_task_id: &str,
    timeout: Duration,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let frame = next_json(stream, remaining).await?;
        let kind = frame.get("type")?.as_str()?;
        if kind == "event" {
            let payload = frame.get("envelope")?.get("payload")?;
            if let Some(task_id) = payload
                .get("task")
                .and_then(|t| t.get("id"))
                .and_then(|v| v.as_str())
            {
                if task_id == expected_task_id {
                    return Some(frame);
                }
            }
        }
        // Ignore hello/snapshot/ping/etc — keep waiting.
    }
}

// ── AC-3: per-project multiplexing ────────────────────────────────────────────

#[tokio::test]
async fn ac3_per_project_multiplexing() {
    let server = spawn_server().await;

    // `TaskCreated` carries `project_id` inline, so the WS filter resolves
    // it without a DB lookup — no need to seed the project projection.
    let p1 = ProjectId::new();
    let p2 = ProjectId::new();

    // ── Client A — only project P1 ───────────────────────────────────────────
    let (mut a_sink, mut a_stream) = connect_ws(server.addr, &server.token).await;
    // Hello first.
    let hello_a = next_json(&mut a_stream, Duration::from_secs(2))
        .await
        .expect("client A must receive Hello");
    assert_eq!(hello_a["type"], "hello");

    a_sink
        .send(Message::Text(
            json!({"type":"subscribe","projects":[p1.as_uuid().to_string()]})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    // ── Client B — only project P2 ───────────────────────────────────────────
    let (mut b_sink, mut b_stream) = connect_ws(server.addr, &server.token).await;
    let hello_b = next_json(&mut b_stream, Duration::from_secs(2))
        .await
        .expect("client B must receive Hello");
    assert_eq!(hello_b["type"], "hello");

    b_sink
        .send(Message::Text(
            json!({"type":"subscribe","projects":[p2.as_uuid().to_string()]})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    // Give both subscriptions a moment to register before producing events.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // ── Drive: create one task in each project ──────────────────────────────
    let task_in_p1 = {
        let mut nt = NewTask::new("task in P1");
        nt.project_id = Some(p1);
        nt
    };
    let task_in_p2 = {
        let mut nt = NewTask::new("task in P2");
        nt.project_id = Some(p2);
        nt
    };

    let envs_p1 = server
        .commands
        .dispatch(Command::CreateTask { task: task_in_p1 }, Actor::user())
        .await
        .unwrap();
    // `TaskId` is `#[serde(transparent)]` over `Uuid`, so the wire format
    // is the bare UUID string — match that, not the display-friendly
    // `tsk_…` form.
    let task_p1_id = match &envs_p1[0].payload {
        Event::TaskCreated { task } => task.id.unwrap().as_uuid().to_string(),
        _ => panic!("expected TaskCreated"),
    };

    let envs_p2 = server
        .commands
        .dispatch(Command::CreateTask { task: task_in_p2 }, Actor::user())
        .await
        .unwrap();
    let task_p2_id = match &envs_p2[0].payload {
        Event::TaskCreated { task } => task.id.unwrap().as_uuid().to_string(),
        _ => panic!("expected TaskCreated"),
    };

    // ── Verify isolation ────────────────────────────────────────────────────
    let a_got_p1 = drain_until_event(&mut a_stream, &task_p1_id, Duration::from_secs(2)).await;
    assert!(a_got_p1.is_some(), "client A must receive its P1 task");

    let b_got_p2 = drain_until_event(&mut b_stream, &task_p2_id, Duration::from_secs(2)).await;
    assert!(b_got_p2.is_some(), "client B must receive its P2 task");

    // Now poll briefly to ensure no cross-project leak. We allow up to 300 ms
    // for any leaked event to arrive; absence is the assertion.
    let a_leak = drain_until_event(&mut a_stream, &task_p2_id, Duration::from_millis(300)).await;
    assert!(a_leak.is_none(), "client A must NOT receive P2 task");

    let b_leak = drain_until_event(&mut b_stream, &task_p1_id, Duration::from_millis(300)).await;
    assert!(b_leak.is_none(), "client B must NOT receive P1 task");
}
