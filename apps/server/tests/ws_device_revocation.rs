//! WS device revocation races.
//!
//! The barriers are `TokenStore` / `EventStore` wrappers, so the production
//! connection path runs unchanged and stops exactly at the awaited store call
//! inside the old window. Nothing here waits on a sleep to hit the race.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use daruma_auth::{
    generate, ApiToken, Capability, NewTokenSpec, ProjectFilter, TokenKind, TokenScope, TokenStore,
};
use daruma_events::{EventEnvelope, EventStore};
use daruma_server::{routes::router, state::AppState};
use daruma_shared::{AgentId, DeviceId, EventId, ProjectId, Result, TokenId};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_tungstenite::{connect_async, tungstenite::Message};

mod common;
use common::{test_app, TestApp};

type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type WsStream = futures::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

/// Upper bound for a failing assertion only; passing runs never wait it out.
const DEADLINE: Duration = Duration::from_secs(5);

// ── Barriers ──────────────────────────────────────────────────────────────────

/// One-shot pause: the first armed store call parks until the test releases it.
struct Barrier {
    armed: AtomicBool,
    reached: Semaphore,
    release: Semaphore,
}

impl Barrier {
    fn armed() -> Arc<Self> {
        Arc::new(Self {
            armed: AtomicBool::new(true),
            reached: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }

    async fn pause_here(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.reached.add_permits(1);
            self.release.acquire().await.unwrap().forget();
        }
    }

    async fn wait_reached(&self) {
        tokio::time::timeout(DEADLINE, self.reached.acquire())
            .await
            .expect("connection never reached the barrier")
            .unwrap()
            .forget();
    }

    fn release(&self) {
        self.release.add_permits(1);
    }
}

/// `verify_bearer` touches the device only after it has classified the token,
/// so pausing here holds a connection past its now stale authentication.
struct PausingTokenStore {
    inner: Arc<dyn TokenStore>,
    barrier: Arc<Barrier>,
}

#[async_trait]
impl TokenStore for PausingTokenStore {
    async fn insert(&self, token: ApiToken) -> Result<()> {
        self.inner.insert(token).await
    }

    async fn list_by_prefix(&self, prefix: &str) -> Result<Vec<ApiToken>> {
        self.inner.list_by_prefix(prefix).await
    }

    async fn get(&self, id: TokenId) -> Result<Option<ApiToken>> {
        self.inner.get(id).await
    }

    async fn list_for_agent(&self, agent_id: AgentId) -> Result<Vec<ApiToken>> {
        self.inner.list_for_agent(agent_id).await
    }

    async fn revoke(&self, id: TokenId) -> Result<bool> {
        self.inner.revoke(id).await
    }

    async fn touch_last_used(&self, id: TokenId) -> Result<()> {
        self.inner.touch_last_used(id).await
    }

    async fn touch_device_last_seen(&self, id: DeviceId) -> Result<()> {
        let touched = self.inner.touch_device_last_seen(id).await;
        self.barrier.pause_here().await;
        touched
    }

    async fn count_active(&self) -> Result<u64> {
        self.inner.count_active().await
    }
}

/// `Hello` reads `latest_seq`, so pausing here holds a connection that has
/// passed admission but has not reached its main loop yet.
struct PausingEventStore {
    inner: Arc<dyn EventStore>,
    barrier: Arc<Barrier>,
}

#[async_trait]
impl EventStore for PausingEventStore {
    async fn append(&self, envelope: EventEnvelope) -> Result<EventEnvelope> {
        self.inner.append(envelope).await
    }

    async fn append_batch(&self, envelopes: Vec<EventEnvelope>) -> Result<Vec<EventEnvelope>> {
        self.inner.append_batch(envelopes).await
    }

    async fn load_since(&self, since_seq: u64, limit: usize) -> Result<Vec<EventEnvelope>> {
        self.inner.load_since(since_seq, limit).await
    }

    async fn load_by_id(&self, id: EventId) -> Result<Option<EventEnvelope>> {
        self.inner.load_by_id(id).await
    }

    async fn latest_seq(&self) -> Result<u64> {
        self.barrier.pause_here().await;
        self.inner.latest_seq().await
    }
}

// ── Fixtures ──────────────────────────────────────────────────────────────────

async fn serve(state: AppState) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app: Router = router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn serve_paused_in_auth(app: &TestApp, barrier: Arc<Barrier>) -> SocketAddr {
    let mut state = app.state.clone();
    state.auth_store = Arc::new(PausingTokenStore {
        inner: app.state.auth_store.clone(),
        barrier,
    });
    serve(state).await
}

async fn serve_paused_before_hello(app: &TestApp, barrier: Arc<Barrier>) -> SocketAddr {
    let mut state = app.state.clone();
    state.store = Arc::new(PausingEventStore {
        inner: app.state.store.clone(),
        barrier,
    });
    serve(state).await
}

async fn device_token(app: &TestApp, scope: TokenScope) -> (DeviceId, String) {
    let device = app
        .state
        .devices
        .insert(DeviceId::new(), "test device")
        .await
        .unwrap();
    let mut secret = generate(NewTokenSpec {
        kind: TokenKind::Pat,
        agent_id: AgentId::new(),
        scope,
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
    (device.id, secret.plaintext)
}

async fn connect(addr: SocketAddr, token: &str) -> (WsSink, WsStream) {
    let url = format!("ws://{addr}/v1/ws?token={token}");
    let (stream, _) = connect_async(url).await.expect("WS connect");
    stream.split()
}

async fn revoke_over_http(addr: SocketAddr, app: &TestApp, device_id: DeviceId) {
    let status = reqwest::Client::new()
        .post(format!("http://{addr}/v1/devices/{device_id}/revoke"))
        .bearer_auth(&app.admin_token)
        .send()
        .await
        .unwrap()
        .status();
    assert!(status.is_success(), "revoke status: {status}");
}

fn subscribe_frame() -> Message {
    Message::Text(
        json!({ "type": "subscribe", "since_seq": 0 })
            .to_string()
            .into(),
    )
}

fn create_task_frame(title: &str) -> Message {
    Message::Text(
        json!({
            "type": "dispatch",
            "client_event_id": EventId::new().as_uuid().to_string(),
            "command": { "type": "create_task", "task": { "title": title } }
        })
        .to_string()
        .into(),
    )
}

/// Everything the server wrote until it closed the socket.
struct Closed {
    frames: Vec<Value>,
    close_code: Option<u16>,
}

impl Closed {
    fn types(&self) -> Vec<&str> {
        self.frames
            .iter()
            .filter_map(|frame| frame["type"].as_str())
            .collect()
    }

    /// The socket got the unauthorized refusal and nothing else from the server.
    fn assert_refused_only(&self) {
        assert_eq!(
            self.close_code,
            Some(1008),
            "expected a policy close, got frames {:?}",
            self.frames
        );
        for frame in &self.frames {
            assert_eq!(frame["type"], "error", "frame leaked: {frame}");
            assert_eq!(frame["code"], "unauthorized", "frame leaked: {frame}");
        }
    }
}

async fn read_until_closed(stream: &mut WsStream) -> Closed {
    let mut closed = Closed {
        frames: Vec::new(),
        close_code: None,
    };
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, stream.next()).await {
            Err(_) => panic!(
                "socket stayed open after revoke, frames so far: {:?}",
                closed.frames
            ),
            Ok(None | Some(Err(_))) => return closed,
            Ok(Some(Ok(Message::Close(frame)))) => {
                closed.close_code = frame.map(|frame| u16::from(frame.code));
                return closed;
            }
            Ok(Some(Ok(Message::Text(text)))) => {
                closed.frames.push(serde_json::from_str(&text).unwrap());
            }
            Ok(Some(Ok(_))) => {}
        }
    }
}

async fn next_of_type(stream: &mut WsStream, wanted: &str) -> Value {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = tokio::time::timeout(remaining, stream.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {wanted}"))
            .expect("socket ended")
            .expect("socket error");
        if let Message::Text(text) = frame {
            let json: Value = serde_json::from_str(&text).unwrap();
            if json["type"] == wanted {
                return json;
            }
        }
    }
}

async fn task_exists(app: &TestApp, title: &str) -> bool {
    app.state
        .tasks
        .list_all()
        .await
        .unwrap()
        .iter()
        .any(|task| task.title == title)
}

// ── Revoke inside the connection window ───────────────────────────────────────

#[tokio::test]
async fn revoke_after_authentication_refuses_the_connection() {
    let app = test_app().await;
    let (device_id, token) = device_token(&app, TokenScope::default_user()).await;
    let barrier = Barrier::armed();
    let addr = serve_paused_in_auth(&app, barrier.clone()).await;

    let (mut sink, mut stream) = connect(addr, &token).await;
    barrier.wait_reached().await;
    sink.send(subscribe_frame()).await.unwrap();
    sink.send(create_task_frame("sent inside the window"))
        .await
        .unwrap();

    revoke_over_http(addr, &app, device_id).await;
    barrier.release();

    read_until_closed(&mut stream).await.assert_refused_only();
    assert!(!task_exists(&app, "sent inside the window").await);
    assert!(app.state.hub.connected_devices().is_empty());
}

#[tokio::test]
async fn revoke_the_hub_never_saw_is_found_by_the_authoritative_recheck() {
    let app = test_app().await;
    let (device_id, token) = device_token(&app, TokenScope::default_user()).await;
    let barrier = Barrier::armed();
    let addr = serve_paused_in_auth(&app, barrier.clone()).await;

    let (mut sink, mut stream) = connect(addr, &token).await;
    barrier.wait_reached().await;
    sink.send(create_task_frame("no broadcast")).await.unwrap();

    assert!(app.state.devices.revoke(device_id).await.unwrap());
    barrier.release();

    read_until_closed(&mut stream).await.assert_refused_only();
    assert!(!task_exists(&app, "no broadcast").await);
}

#[tokio::test]
async fn revoke_after_admission_stops_hello_commands_and_events() {
    let app = test_app().await;
    let (device_id, token) = device_token(&app, TokenScope::default_user()).await;
    let barrier = Barrier::armed();
    let addr = serve_paused_before_hello(&app, barrier.clone()).await;

    let (mut sink, mut stream) = connect(addr, &token).await;
    barrier.wait_reached().await;
    sink.send(subscribe_frame()).await.unwrap();
    sink.send(create_task_frame("sent before hello"))
        .await
        .unwrap();

    revoke_over_http(addr, &app, device_id).await;
    barrier.release();

    read_until_closed(&mut stream).await.assert_refused_only();
    assert!(!task_exists(&app, "sent before hello").await);
}

#[tokio::test]
async fn revocation_lost_to_broadcast_lag_still_closes_the_connection() {
    let app = test_app().await;
    let (device_id, token) = device_token(&app, TokenScope::default_user()).await;
    let barrier = Barrier::armed();
    let addr = serve_paused_before_hello(&app, barrier.clone()).await;

    let (_sink, mut stream) = connect(addr, &token).await;
    barrier.wait_reached().await;

    revoke_over_http(addr, &app, device_id).await;
    for _ in 0..256 {
        app.state.hub.notify_device_revoked(DeviceId::new());
    }
    barrier.release();

    read_until_closed(&mut stream).await.assert_refused_only();
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while !app.state.hub.connected_devices().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "main loop outlived the revoked device"
        );
        tokio::task::yield_now().await;
    }
}

// ── Revoke before and after the window ────────────────────────────────────────

#[tokio::test]
async fn revoke_before_authentication_rejects_a_new_connection() {
    let app = test_app().await;
    let (device_id, token) = device_token(&app, TokenScope::default_user()).await;
    let addr = serve(app.state.clone()).await;
    revoke_over_http(addr, &app, device_id).await;

    let (mut sink, mut stream) = connect(addr, &token).await;
    let _ = sink.send(create_task_frame("already revoked")).await;

    let closed = read_until_closed(&mut stream).await;
    assert_eq!(closed.close_code, Some(1008));
    assert!(closed.frames.is_empty(), "frames: {:?}", closed.frames);
    assert!(!task_exists(&app, "already revoked").await);
}

#[tokio::test]
async fn revoke_after_subscribe_stops_commands_and_events_for_that_device_only() {
    let app = test_app().await;
    let (revoked_id, revoked_token) = device_token(&app, TokenScope::default_user()).await;
    let (other_id, other_token) = device_token(&app, TokenScope::default_user()).await;
    let addr = serve(app.state.clone()).await;

    let (mut revoked_sink, mut revoked_stream) = connect(addr, &revoked_token).await;
    let (mut other_sink, mut other_stream) = connect(addr, &other_token).await;
    let (mut admin_sink, mut admin_stream) = connect(addr, &app.admin_token).await;
    for (sink, stream) in [
        (&mut revoked_sink, &mut revoked_stream),
        (&mut other_sink, &mut other_stream),
        (&mut admin_sink, &mut admin_stream),
    ] {
        next_of_type(stream, "hello").await;
        sink.send(subscribe_frame()).await.unwrap();
        next_of_type(stream, "snapshot").await;
    }

    revoke_over_http(addr, &app, revoked_id).await;
    // Either the frame is refused on ingress or the socket is already gone.
    let _ = revoked_sink.send(create_task_frame("after revoke")).await;
    admin_sink
        .send(create_task_frame("seen by the others"))
        .await
        .unwrap();

    next_of_type(&mut other_stream, "event").await;
    next_of_type(&mut admin_stream, "event").await;
    let closed = read_until_closed(&mut revoked_stream).await;
    closed.assert_refused_only();
    assert!(!closed.types().contains(&"ack"));
    assert!(!task_exists(&app, "after revoke").await);

    other_sink
        .send(create_task_frame("other device still works"))
        .await
        .unwrap();
    next_of_type(&mut other_stream, "ack").await;
    assert!(task_exists(&app, "other device still works").await);
    assert!(!app.state.hub.is_device_revoked(other_id));
}

#[tokio::test]
async fn project_scoped_device_token_stays_refused() {
    let app = test_app().await;
    let mut scope = TokenScope::default_user();
    scope.projects = ProjectFilter::Only {
        projects: vec![ProjectId::new()],
    };
    assert!(scope.capabilities.has(Capability::TaskWrite));
    let (_device_id, token) = device_token(&app, scope).await;
    let addr = serve(app.state.clone()).await;

    let (mut sink, mut stream) = connect(addr, &token).await;
    let _ = sink.send(create_task_frame("project scoped")).await;

    let closed = read_until_closed(&mut stream).await;
    assert!(closed.frames.is_empty(), "frames: {:?}", closed.frames);
    assert!(!task_exists(&app, "project scoped").await);
    assert!(app.state.hub.connected_devices().is_empty());
}
