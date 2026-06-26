//! §3.9.4 — `WS Hub` per-subscriber `mpsc` fanout integration test.
//!
//! Connects four WebSocket clients to a live server:
//!
//! * `slow` — subscribes and then **stops reading frames**, holding the
//!   stream open. Simulates a stalled / suspended browser tab.
//! * `fast_1`, `fast_2`, `fast_3` — drain their streams as fast as they
//!   can. Per the §3.9.4 design these MUST be unaffected by the slow
//!   consumer: each has its own per-subscriber mpsc channel, so a stall
//!   on one path does not cascade.
//!
//! ## What this test asserts (the integration property)
//!
//! We drive a burst of `BURST` task creations through the in-process
//! `CommandBus` and assert that every fast client receives **all
//! `BURST` events, in publish order, with no `Resync` frame**. This is
//! the critical non-cascade property: prior to §3.9.4 a slow receiver
//! shared a broadcast ring buffer with every other subscriber, so
//! `RecvError::Lagged` on one path would trigger `Resync` emission on
//! every path. With per-subscriber mpsc this can no longer happen.
//!
//! ## What this test does NOT assert
//!
//! That the slow subscriber's socket is *eventually* closed by the
//! server. Forcing real TCP back-pressure to overflow the bounded
//! `out_tx` (capacity 64) requires a burst large enough to saturate the
//! Linux autotuned receive buffer (often 6 MB+) — unreliable across
//! CI environments. The slow-drop mechanism itself is verified
//! deterministically by the `slow_subscriber_dropped_on_full` unit test
//! in `crates/sync/src/hub.rs`, which exercises the same code path
//! directly against the `Arc<DashMap>` + `mpsc::Sender` map without
//! relying on TCP buffer sizes.

use std::net::SocketAddr;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use daruma_core::{Command, CommandBus};
use daruma_domain::{Actor, NewTask};
use tokio_tungstenite::{connect_async, tungstenite::Message};

mod common;
use common::{spawn_server as spawn_test_server, test_app};

const BURST: u64 = 128; // 2× WS_SUBSCRIBER_CHANNEL — comfortably over the bound.

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

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_and_subscribe(addr: SocketAddr, token: &str) -> WsStream {
    let url = format!("ws://{addr}/v1/ws?token={token}");
    let (mut stream, _resp) = connect_async(&url).await.expect("WS connect");

    // Consume Hello.
    let hello = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("Hello must arrive in time")
        .expect("Hello must be Some")
        .expect("Hello must be Ok");
    match hello {
        Message::Text(t) => {
            let v: Value = serde_json::from_str(&t).expect("Hello JSON");
            assert_eq!(v["type"], "hello", "first frame must be Hello: got {v}");
        }
        other => panic!("expected text Hello, got {other:?}"),
    }

    // Subscribe (defaults to channel=[Tasks], no since_seq).
    stream
        .send(Message::Text(
            json!({"type":"subscribe"}).to_string().into(),
        ))
        .await
        .expect("Subscribe must send");

    stream
}

#[tokio::test]
async fn slow_consumer_does_not_drop_fast_peers() {
    let server = spawn_server().await;

    // ── slow subscriber: connects, subscribes, then *never reads again*. ──
    //
    // We hold the stream as `_slow` for the lifetime of the test so it
    // doesn't get closed prematurely. The test runtime never pumps frames
    // out of it, simulating a stalled / suspended browser tab.
    let slow_stream = connect_and_subscribe(server.addr, &server.token).await;
    let (_slow_sink, slow_stream_rx) = slow_stream.split();

    // ── fast subscribers ──
    let mut fast_streams = Vec::with_capacity(3);
    for _ in 0..3 {
        let s = connect_and_subscribe(server.addr, &server.token).await;
        fast_streams.push(s);
    }

    // Give all four subscriptions a moment to wire up the Hub fanout
    // entries before the publisher starts.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // ── drive the burst via the in-process CommandBus ──
    let mut created_ids = Vec::with_capacity(BURST as usize);
    for i in 0..BURST {
        let task = NewTask::new(format!("burst-{i}"));
        let envs = server
            .commands
            .dispatch(Command::CreateTask { task }, Actor::user())
            .await
            .expect("dispatch CreateTask must succeed");
        // Capture the task id so we can match frames precisely.
        let id = match &envs[0].payload {
            daruma_events::Event::TaskCreated { task } => task
                .id
                .expect("TaskCreated must carry id")
                .as_uuid()
                .to_string(),
            other => panic!("expected TaskCreated, got {other:?}"),
        };
        created_ids.push(id);
    }

    // ── drain each fast stream, collecting per-stream Event-task-ids ──
    async fn drain_events(
        stream: WsStream,
        expected: usize,
        timeout: Duration,
    ) -> (Vec<String>, bool /* saw_resync */) {
        let (mut _sink, mut rx) = stream.split();
        let deadline = tokio::time::Instant::now() + timeout;
        let mut got: Vec<String> = Vec::with_capacity(expected);
        let mut saw_resync = false;
        while got.len() < expected && tokio::time::Instant::now() < deadline {
            let remaining = deadline - tokio::time::Instant::now();
            let frame = match tokio::time::timeout(remaining, rx.next()).await {
                Ok(Some(Ok(m))) => m,
                _ => break,
            };
            let text = match frame {
                Message::Text(t) => t,
                _ => continue,
            };
            let json: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match json["type"].as_str() {
                Some("event") => {
                    if let Some(id) = json["envelope"]["payload"]["task"]["id"].as_str() {
                        got.push(id.to_string());
                    }
                }
                Some("resync") => saw_resync = true,
                _ => {} // hello/snapshot/ping/etc — ignore
            }
        }
        (got, saw_resync)
    }

    // Generous deadline — the fast streams must catch up regardless of
    // the slow consumer's behaviour. 5s is well above the no-op steady
    // state on a CI box; if a test box hits this we have a real bug.
    let timeout = Duration::from_secs(5);
    let mut handles = Vec::new();
    for s in fast_streams {
        handles.push(tokio::spawn(drain_events(s, BURST as usize, timeout)));
    }

    let mut fast_results = Vec::with_capacity(3);
    for h in handles {
        fast_results.push(h.await.expect("drainer task panicked"));
    }

    for (idx, (got, saw_resync)) in fast_results.iter().enumerate() {
        assert_eq!(
            got.len(),
            BURST as usize,
            "fast client #{idx} must receive all {BURST} events (got {})",
            got.len()
        );
        assert!(
            !saw_resync,
            "fast client #{idx} must NOT see a Resync frame on the §3.9.4 path"
        );
        // Order must match the produced order (per-subscriber FIFO).
        assert_eq!(
            got, &created_ids,
            "fast client #{idx} must receive events in publish order"
        );
    }

    // Keep `slow_stream_rx` alive until the very end so the slow
    // subscription stays registered on the server through the burst.
    // (We do NOT assert on close here — see the module doc-comment for
    // why; the unit test in `crates/sync/src/hub.rs` covers the
    // slow-drop mechanism deterministically.)
    drop(slow_stream_rx);
    drop(_slow_sink);
}
