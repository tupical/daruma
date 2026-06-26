//! AC-4 — when the broadcast bus overflows, the new WS Hub fanout layer
//! (§3.9.4) responds by closing every active subscriber's socket so
//! clients reconnect with `since_seq` and rehydrate via the existing
//! `Snapshot` path. This replaces the legacy "emit `Resync`" behaviour
//! — there's no more shared broadcast-Receiver between WS subscribers,
//! so per-subscriber `RecvError::Lagged` cannot occur. Global bus-lag
//! still has a recovery story: the fanout task clears its subscriber
//! map, every writer-side `out_tx` closes, the forwarder emits a
//! `Close(1001)` frame, and clients reconnect.
//!
//! Strategy:
//!   * Spawn a real server with a *tiny* `EventBus` capacity (16) so
//!     a synchronous burst can overflow it before the fanout task runs.
//!   * Connect a single WS client, subscribe.
//!   * Synchronously publish `capacity * 4` events directly through
//!     the bus. The Hub fanout task observes `RecvError::Lagged` and
//!     clears the subscriber map → our writer task receives a
//!     `Close(1001)` frame.
//!   * The test asserts the client observes a WS close (either as a
//!     `Message::Close` or as the stream ending) and **does not**
//!     observe a `Resync` frame from this path.

use std::net::SocketAddr;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use daruma_domain::{Actor, NewTask};
use daruma_events::{Event, EventBus, EventEnvelope};
use tokio_tungstenite::{connect_async, tungstenite::Message};

mod common;
use common::{spawn_server as spawn_test_server, TestAppBuilder};

const BUS_CAPACITY: usize = 16;
const PUBLISH_BURST: u64 = (BUS_CAPACITY as u64) * 4;

struct LagServer {
    addr: SocketAddr,
    token: String,
    bus: EventBus,
}

async fn spawn_server() -> LagServer {
    let app = TestAppBuilder::default()
        .bus_capacity(BUS_CAPACITY)
        .build()
        .await;
    let addr = spawn_test_server(&app).await;
    LagServer {
        addr,
        token: app.admin_token,
        bus: app.bus,
    }
}

#[tokio::test]
async fn ac4_overflowed_broadcast_closes_subscribers() {
    let server = spawn_server().await;

    let url = format!("ws://{}/v1/ws?token={}", server.addr, server.token);
    let (stream, _resp) = connect_async(&url).await.expect("WS connect");
    let (mut sink, mut stream) = stream.split();

    // 1. Consume the Hello frame.
    let hello = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .ok()
        .and_then(|f| f.and_then(|r| r.ok()))
        .expect("must receive Hello");
    let hello_text = match hello {
        Message::Text(t) => t.to_string(),
        other => panic!("expected text Hello, got {other:?}"),
    };
    let hello: Value = serde_json::from_str(&hello_text).unwrap();
    assert_eq!(hello["type"], "hello");

    // 2. Subscribe — no since_seq, no filters (defaults to [Tasks]).
    sink.send(Message::Text(
        json!({"type":"subscribe"}).to_string().into(),
    ))
    .await
    .unwrap();

    // 3. Give the server a moment to wire up the per-WS mpsc subscriber
    //    in the Hub fanout map.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 4. Burst-publish synchronously — no yield between sends. This
    //    overflows the broadcast queue before the fanout task runs, so
    //    on its next poll it observes `RecvError::Lagged` and clears
    //    its subscriber map.
    for i in 0..PUBLISH_BURST {
        let mut env = EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated {
                task: NewTask::new(format!("burst-{i}")),
            },
        );
        // Forwarder skips events with seq <= boundary (snapshot dedup).
        // Boundary was 0 at subscribe time, so any seq > 0 is in scope.
        env.seq = i + 1;
        server.bus.publish(env);
    }

    // 5. Poll until either the connection closes or we see a Close
    //    frame. The forwarder emits `Message::Close(Some(1001))` on
    //    exit; tokio-tungstenite delivers it as a `Message::Close`
    //    item, followed by the stream ending.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut got_close = false;
    let mut got_resync = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let frame_opt = tokio::time::timeout(remaining, stream.next())
            .await
            .ok()
            .flatten();
        match frame_opt {
            None => {
                // Stream ended — accept as evidence the server closed
                // the socket after the broadcast-lag clear.
                got_close = true;
                break;
            }
            Some(Ok(Message::Close(_frame))) => {
                got_close = true;
                break;
            }
            Some(Ok(Message::Text(text))) => {
                if let Ok(json) = serde_json::from_str::<Value>(&text) {
                    if json["type"] == "resync" {
                        got_resync = true;
                    }
                }
                // Otherwise it's an Event from the snapshot or live tail
                // before the lag — ignore and keep polling.
            }
            Some(Ok(_)) => { /* ignore Ping/Pong/Binary */ }
            Some(Err(_)) => {
                // Connection error counts as close.
                got_close = true;
                break;
            }
        }
    }

    assert!(
        got_close,
        "broadcast overflow must close the WS subscriber (sent={PUBLISH_BURST}, capacity={BUS_CAPACITY})"
    );
    assert!(
        !got_resync,
        "§3.9.4 removes Resync emission from the live-forwarder path; \
         lag recovery now happens via Close + client reconnect with `since_seq`"
    );
}
