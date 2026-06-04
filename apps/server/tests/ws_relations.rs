//! AC-8 — WS `Channel::Tasks` streams `TaskLinked / TaskUnlinked / TaskUnblocked`.
//!
//! Tests:
//!   * `subscribe_tasks_receives_link_events` — WS subscriber with `SubscribeTasks`
//!     receives `TaskLinked` when a relation is created via REST.
//!   * `subscribe_tasks_receives_unlinked_event` — subscriber receives `TaskUnlinked`
//!     when a relation is deleted via REST.
//!   * `subscribe_tasks_receives_unblocked_event` — subscriber receives both
//!     `TaskStatusChanged` and `TaskUnblocked` when the sole blocker is set Done.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use taskagent_auth::{
    generate, Capabilities, Capability, NewTokenSpec, ProjectFilter, TokenKind, TokenScope,
    TokenStore,
};
use taskagent_shared::AgentId;
use tokio_tungstenite::{connect_async, tungstenite::Message};

mod common;
use common::{spawn_server as spawn_test_server, test_app};

// ── Server scaffold ───────────────────────────────────────────────────────────

struct RelationsServer {
    addr: SocketAddr,
    admin_token: String,
    auth_store: Arc<dyn TokenStore>,
}

async fn spawn_server() -> RelationsServer {
    let app = test_app().await;
    let addr = spawn_test_server(&app).await;
    RelationsServer {
        addr,
        admin_token: app.admin_token,
        auth_store: app.state.auth_store.clone(),
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

/// Skip non-event frames and return the first event frame with the given kind,
/// or `None` on timeout.
async fn drain_for_kind(stream: &mut WsStream, kind: &str, timeout: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let frame = next_json(stream, remaining).await?;
        if frame.get("type").and_then(|v| v.as_str()) != Some("event") {
            continue;
        }
        let payload_type = frame["envelope"]["payload"]["type"].as_str().unwrap_or("");
        // kind() uses dot-notation for relation events ("task.linked") but the
        // serde tag uses snake_case ("task_linked"). Match on either form.
        let snake = kind.replace('.', "_");
        if payload_type == kind || payload_type == snake {
            return Some(frame);
        }
    }
}

/// HTTP POST helper using reqwest (real TCP, since the server is listening).
async fn http_post(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let resp = client
        .post(url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .expect("POST succeeds");
    let status = resp.status().as_u16();
    let json = resp
        .json::<serde_json::Value>()
        .await
        .unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn http_delete(client: &reqwest::Client, url: &str, token: &str) -> (u16, serde_json::Value) {
    let resp = client
        .delete(url)
        .bearer_auth(token)
        .send()
        .await
        .expect("DELETE succeeds");
    let status = resp.status().as_u16();
    let json = resp
        .json::<serde_json::Value>()
        .await
        .unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Create a task via the command endpoint and return its ID string.
async fn create_task(client: &reqwest::Client, base_url: &str, token: &str, title: &str) -> String {
    let (status, resp) = http_post(
        client,
        &format!("{base_url}/v1/commands"),
        token,
        json!({"command": {"type": "create_task", "task": {"title": title}}}),
    )
    .await;
    assert_eq!(status, 200, "create_task failed: {resp}");
    resp["data"]
        .as_array()
        .expect("data array")
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == "task_created" {
                p.get("task")?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("task_created event with task.id")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// AC-8 part 1: WS subscriber with `SubscribeTasks` receives `TaskLinked`
/// when a relation is created via `POST /v1/relations`.
#[tokio::test]
async fn subscribe_tasks_receives_link_events() {
    let server = spawn_server().await;
    let client = reqwest::Client::new();
    let base_url = format!("http://{}", server.addr);

    // Use a token with SubscribeTasks capability.
    let sub_token = make_token(&server.auth_store, [Capability::SubscribeTasks].into()).await;

    // Connect the WS subscriber and subscribe to the tasks channel.
    let (mut sink, mut stream) = connect_ws(server.addr, &sub_token).await;
    let _ = next_json(&mut stream, Duration::from_secs(2)).await; // consume Hello
    sink.send(Message::Text(
        json!({"type": "subscribe", "channels": ["tasks"]})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(150)).await;

    // Create two tasks and link them.
    let from = create_task(&client, &base_url, &server.admin_token, "Blocker WS").await;
    let to = create_task(&client, &base_url, &server.admin_token, "Blocked WS").await;

    let (link_status, link_resp) = http_post(
        &client,
        &format!("{base_url}/v1/relations"),
        &server.admin_token,
        json!({"from": from, "to": to, "kind": "blocks"}),
    )
    .await;
    assert_eq!(link_status, 201, "POST /v1/relations failed: {link_resp}");

    // The subscriber must receive a TaskLinked event.
    let frame = drain_for_kind(&mut stream, "task_linked", Duration::from_secs(3)).await;
    assert!(
        frame.is_some(),
        "subscriber must receive TaskLinked on tasks channel"
    );
    let payload = &frame.unwrap()["envelope"]["payload"];
    assert_eq!(
        payload["from"].as_str().unwrap(),
        from,
        "TaskLinked.from must match"
    );
    assert_eq!(
        payload["to"].as_str().unwrap(),
        to,
        "TaskLinked.to must match"
    );
    assert_eq!(
        payload["kind"].as_str().unwrap(),
        "blocks",
        "TaskLinked.kind must be blocks"
    );
}

/// AC-8 part 2: WS subscriber receives `TaskUnlinked` when a relation is
/// deleted via `DELETE /v1/relations/{id}`.
#[tokio::test]
async fn subscribe_tasks_receives_unlinked_event() {
    let server = spawn_server().await;
    let client = reqwest::Client::new();
    let base_url = format!("http://{}", server.addr);

    let sub_token = make_token(&server.auth_store, [Capability::SubscribeTasks].into()).await;
    let (mut sink, mut stream) = connect_ws(server.addr, &sub_token).await;
    let _ = next_json(&mut stream, Duration::from_secs(2)).await;
    sink.send(Message::Text(
        json!({"type": "subscribe", "channels": ["tasks"]})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(150)).await;

    // Create tasks and a relation.
    let from = create_task(&client, &base_url, &server.admin_token, "From Unlink").await;
    let to = create_task(&client, &base_url, &server.admin_token, "To Unlink").await;

    let (_, link_resp) = http_post(
        &client,
        &format!("{base_url}/v1/relations"),
        &server.admin_token,
        json!({"from": from, "to": to, "kind": "relates_to"}),
    )
    .await;
    let relation_id = link_resp["data"]["relation_id"]
        .as_str()
        .expect("relation_id from create")
        .to_owned();

    // Drain the TaskLinked event so it doesn't interfere.
    let _ = drain_for_kind(&mut stream, "task_linked", Duration::from_secs(2)).await;

    // Delete the relation.
    let (del_status, del_resp) = http_delete(
        &client,
        &format!("{base_url}/v1/relations/{relation_id}"),
        &server.admin_token,
    )
    .await;
    assert_eq!(del_status, 200, "DELETE relation failed: {del_resp}");

    // The subscriber must receive TaskUnlinked.
    let frame = drain_for_kind(&mut stream, "task_unlinked", Duration::from_secs(3)).await;
    assert!(
        frame.is_some(),
        "subscriber must receive TaskUnlinked on tasks channel"
    );
    let payload = &frame.unwrap()["envelope"]["payload"];
    assert_eq!(
        payload["from"].as_str().unwrap(),
        from,
        "TaskUnlinked.from must match"
    );
}

/// AC-8 part 3: When blocker A is set Done, subscriber receives both
/// `TaskStatusChanged` (for A) and `TaskUnblocked` (for B).
#[tokio::test]
async fn subscribe_tasks_receives_unblocked_event() {
    let server = spawn_server().await;
    let client = reqwest::Client::new();
    let base_url = format!("http://{}", server.addr);

    let sub_token = make_token(&server.auth_store, [Capability::SubscribeTasks].into()).await;
    let (mut sink, mut stream) = connect_ws(server.addr, &sub_token).await;
    let _ = next_json(&mut stream, Duration::from_secs(2)).await;
    sink.send(Message::Text(
        json!({"type": "subscribe", "channels": ["tasks"]})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(150)).await;

    // A blocks B.
    let a = create_task(&client, &base_url, &server.admin_token, "Blocker A").await;
    let b = create_task(&client, &base_url, &server.admin_token, "Blocked B").await;

    let (link_status, link_resp) = http_post(
        &client,
        &format!("{base_url}/v1/relations"),
        &server.admin_token,
        json!({"from": a, "to": b, "kind": "blocks"}),
    )
    .await;
    assert_eq!(link_status, 201, "POST /v1/relations failed: {link_resp}");

    // Drain TaskCreated×2 + TaskLinked that already arrived.
    let _ = drain_for_kind(&mut stream, "task_linked", Duration::from_secs(2)).await;

    // Set A to Done — should emit TaskStatusChanged(A) + TaskUnblocked(B).
    let (status, resp) = http_post(
        &client,
        &format!("{base_url}/v1/commands"),
        &server.admin_token,
        json!({"command": {"type": "set_status", "id": a, "status": "done"}}),
    )
    .await;
    assert_eq!(status, 200, "set_status done failed: {resp}");

    // Subscriber must receive TaskUnblocked for B.
    let frame = drain_for_kind(&mut stream, "task_unblocked", Duration::from_secs(3)).await;
    assert!(
        frame.is_some(),
        "subscriber must receive TaskUnblocked when sole blocker is set Done"
    );
    let payload = &frame.unwrap()["envelope"]["payload"];
    assert_eq!(
        payload["task_id"].as_str().unwrap(),
        b,
        "TaskUnblocked.task_id must be B"
    );
    assert_eq!(
        payload["unblocked_by"].as_str().unwrap(),
        a,
        "TaskUnblocked.unblocked_by must be A"
    );
}
