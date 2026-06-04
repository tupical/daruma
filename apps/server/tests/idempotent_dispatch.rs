//! WS idempotent dispatch regression tests.

use std::net::SocketAddr;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use taskagent_shared::EventId;
use tokio_tungstenite::{connect_async, tungstenite::Message};

mod common;
use common::{spawn_server as spawn_test_server, test_app, TestApp};

type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type WsStream = futures::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

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

async fn next_ack(stream: &mut WsStream) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for ack");

        let frame = next_json(stream, remaining)
            .await
            .expect("WS frame must be valid JSON");
        match frame.get("type").and_then(|v| v.as_str()) {
            Some("ack") => return frame,
            Some("error") => panic!("unexpected WS error: {frame}"),
            _ => {}
        }
    }
}

async fn spawn_server() -> (TestApp, SocketAddr) {
    let app = test_app().await;
    let addr = spawn_test_server(&app).await;
    (app, addr)
}

#[tokio::test]
async fn duplicate_ws_dispatch_reuses_committed_event() {
    let (app, addr) = spawn_server().await;
    let (mut a_sink, mut a_stream) = connect_ws(addr, &app.admin_token).await;
    let (mut b_sink, mut b_stream) = connect_ws(addr, &app.admin_token).await;

    let hello = next_json(&mut a_stream, Duration::from_secs(2))
        .await
        .expect("client A must receive Hello");
    assert_eq!(hello["type"], "hello");
    let capabilities = hello["capabilities"]
        .as_array()
        .expect("Hello capabilities must be an array");
    assert!(capabilities
        .iter()
        .any(|cap| cap.as_str() == Some("device-sync")));
    assert!(capabilities
        .iter()
        .any(|cap| cap.as_str() == Some("idempotent-dispatch")));

    let hello = next_json(&mut b_stream, Duration::from_secs(2))
        .await
        .expect("client B must receive Hello");
    assert_eq!(hello["type"], "hello");

    let client_event_id = EventId::new();
    let dispatch = Message::Text(
        json!({
            "type": "dispatch",
            "client_event_id": client_event_id.as_uuid().to_string(),
            "command": {
                "type": "create_task",
                "task": {
                    "title": "WS idempotent"
                }
            }
        })
        .to_string()
        .into(),
    );

    let (sent_a, sent_b) = tokio::join!(a_sink.send(dispatch.clone()), b_sink.send(dispatch));
    sent_a.expect("client A dispatch send");
    sent_b.expect("client B dispatch send");

    let (ack_a, ack_b) = tokio::join!(next_ack(&mut a_stream), next_ack(&mut b_stream));
    assert_eq!(ack_a["event_id"], ack_b["event_id"]);

    let tasks = app.state.tasks.list_all().await.unwrap();
    let created = tasks
        .iter()
        .filter(|task| task.title == "WS idempotent")
        .count();
    assert_eq!(created, 1);
}
