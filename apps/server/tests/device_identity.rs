use std::time::Duration;

use daruma_auth::{generate, NewTokenSpec, TokenKind, TokenScope};
use daruma_shared::{AgentId, DeviceId};
use futures::StreamExt;
use serde_json::Value;
use tokio_tungstenite::{connect_async, tungstenite::Message};

mod common;
use common::{spawn_server, test_app};

#[tokio::test]
async fn ws_gets_unauthorized_within_two_seconds_after_device_revoke() {
    let app = test_app().await;
    let device = app
        .state
        .devices
        .insert(DeviceId::new(), "test device")
        .await
        .unwrap();
    let mut secret = generate(NewTokenSpec {
        kind: TokenKind::Pat,
        agent_id: AgentId::new(),
        scope: TokenScope::default_user(),
        rate_limit_per_min: 60,
        expired_at: None,
    })
    .unwrap();
    secret.record.device_id = Some(device.id);
    app.state
        .auth_store
        .insert(secret.record.clone())
        .await
        .unwrap();

    let addr = spawn_server(&app).await;
    let url = format!("ws://{addr}/v1/ws?token={}", secret.plaintext);
    let (mut ws, _) = connect_async(url).await.unwrap();
    assert_eq!(next_json(&mut ws).await["type"], "hello");

    let status = reqwest::Client::new()
        .post(format!("http://{addr}/v1/devices/{}/revoke", device.id))
        .bearer_auth(&app.admin_token)
        .send()
        .await
        .unwrap()
        .status();
    assert!(status.is_success(), "revoke status: {status}");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_unauthorized = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let Some(frame) = tokio::time::timeout(remaining, ws.next())
            .await
            .ok()
            .flatten()
        else {
            break;
        };
        match frame.unwrap() {
            Message::Text(text) => {
                let json: Value = serde_json::from_str(&text).unwrap();
                if json["type"] == "error" && json["code"] == "unauthorized" {
                    saw_unauthorized = true;
                }
            }
            Message::Close(_) if saw_unauthorized => return,
            _ => {}
        }
    }
    assert!(
        saw_unauthorized,
        "socket did not receive unauthorized in time"
    );
}

async fn next_json(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match msg {
        Message::Text(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("expected text frame, got {other:?}"),
    }
}
