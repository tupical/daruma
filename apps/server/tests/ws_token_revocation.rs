//! Revoking one token: open WS sessions, new connections and the HTTP cache.
//!
//! The barriers are `TokenStore` wrappers, so the production path runs
//! unchanged and stops exactly at the awaited store call. A revoke "the hub
//! never saw" goes straight to the store, like another process or a lost
//! notification would. Nothing here waits on a sleep to hit a race.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use daruma_auth::{
    generate, ApiToken, Capabilities, Capability, NewTokenSpec, ProjectFilter, TokenKind,
    TokenScope, TokenStore,
};
use daruma_server::{routes::router, state::AppState};
use daruma_shared::{AgentId, CoreError, DeviceId, EventId, ProjectId, Result, TokenId};
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

/// Parks the `arm`ed n-th call of the store method it guards until released.
struct Barrier {
    countdown: AtomicI64,
    reached: Semaphore,
    release: Semaphore,
}

impl Barrier {
    fn idle() -> Arc<Self> {
        Arc::new(Self {
            countdown: AtomicI64::new(0),
            reached: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }

    fn armed(nth_call: i64) -> Arc<Self> {
        let barrier = Self::idle();
        barrier.countdown.store(nth_call, Ordering::SeqCst);
        barrier
    }

    fn arm(&self, nth_call: i64) {
        self.countdown.store(nth_call, Ordering::SeqCst);
    }

    async fn pause_here(&self) {
        if self.countdown.fetch_sub(1, Ordering::SeqCst) == 1 {
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

/// `get` is the per-frame recheck of one watched token, so a barrier there
/// holds its connection before it reads the row. `touch_last_used` runs after
/// `verify_bearer` classified the token, so a barrier there holds a request
/// past its now stale authentication.
struct InstrumentedTokenStore {
    inner: Arc<dyn TokenStore>,
    watched: Option<TokenId>,
    before_get: Arc<Barrier>,
    after_verify: Arc<Barrier>,
    fail_get: Arc<AtomicBool>,
}

#[async_trait]
impl TokenStore for InstrumentedTokenStore {
    async fn insert(&self, token: ApiToken) -> Result<()> {
        self.inner.insert(token).await
    }

    async fn list_by_prefix(&self, prefix: &str) -> Result<Vec<ApiToken>> {
        self.inner.list_by_prefix(prefix).await
    }

    async fn get(&self, id: TokenId) -> Result<Option<ApiToken>> {
        if self.watched == Some(id) {
            self.before_get.pause_here().await;
            if self.fail_get.load(Ordering::SeqCst) {
                return Err(CoreError::storage("token row unreadable"));
            }
        }
        self.inner.get(id).await
    }

    async fn list_for_agent(&self, agent_id: AgentId) -> Result<Vec<ApiToken>> {
        self.inner.list_for_agent(agent_id).await
    }

    async fn revoke(&self, id: TokenId) -> Result<bool> {
        self.inner.revoke(id).await
    }

    async fn touch_last_used(&self, id: TokenId) -> Result<()> {
        let touched = self.inner.touch_last_used(id).await;
        if self.watched == Some(id) {
            self.after_verify.pause_here().await;
        }
        touched
    }

    async fn touch_device_last_seen(&self, id: DeviceId) -> Result<()> {
        self.inner.touch_device_last_seen(id).await
    }

    async fn count_active(&self) -> Result<u64> {
        self.inner.count_active().await
    }
}

// ── Harness ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Token {
    id: TokenId,
    agent_id: AgentId,
    record: ApiToken,
    plaintext: String,
}

async fn mint(app: &TestApp, device_id: Option<DeviceId>) -> Token {
    let mut secret = generate(NewTokenSpec {
        kind: TokenKind::Pat,
        agent_id: AgentId::new(),
        scope: TokenScope::default_user(),
        rate_limit_per_min: 600,
        expired_at: None,
    })
    .unwrap();
    secret.record.device_id = device_id;
    app.state
        .auth_store
        .insert(secret.record.clone())
        .await
        .unwrap();
    Token {
        id: secret.record.id,
        agent_id: secret.record.agent_id,
        record: secret.record,
        plaintext: secret.plaintext,
    }
}

async fn serve(state: AppState) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app: Router = router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

struct Instrumented {
    addr: SocketAddr,
    before_get: Arc<Barrier>,
    after_verify: Arc<Barrier>,
    fail_get: Arc<AtomicBool>,
}

async fn serve_instrumented(
    app: &TestApp,
    token: &Token,
    before_get: Arc<Barrier>,
    after_verify: Arc<Barrier>,
) -> Instrumented {
    let fail_get = Arc::new(AtomicBool::new(false));
    let mut state = app.state.clone();
    state.auth_store = Arc::new(InstrumentedTokenStore {
        inner: app.state.auth_store.clone(),
        watched: Some(token.id),
        before_get: before_get.clone(),
        after_verify: after_verify.clone(),
        fail_get: fail_get.clone(),
    });
    Instrumented {
        addr: serve(state).await,
        before_get,
        after_verify,
        fail_get,
    }
}

async fn connect(addr: SocketAddr, token: &str) -> (WsSink, WsStream) {
    let url = format!("ws://{addr}/v1/ws?token={token}");
    let (stream, _) = connect_async(url).await.expect("WS connect");
    stream.split()
}

/// A subscribed socket: Hello and the initial snapshot already read.
async fn subscribed(addr: SocketAddr, token: &str) -> (WsSink, WsStream) {
    let (mut sink, mut stream) = connect(addr, token).await;
    next_of_type(&mut stream, "hello").await;
    sink.send(subscribe_frame()).await.unwrap();
    next_of_type(&mut stream, "snapshot").await;
    (sink, stream)
}

async fn delete_token(addr: SocketAddr, app: &TestApp, id: TokenId) {
    let status = reqwest::Client::new()
        .delete(format!("http://{addr}/v1/tokens/{id}"))
        .bearer_auth(&app.admin_token)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 204, "DELETE status");
}

fn subscribe_frame() -> Message {
    Message::Text(
        json!({ "type": "subscribe", "since_seq": 0 })
            .to_string()
            .into(),
    )
}

fn dispatch_frame(command: Value) -> Message {
    Message::Text(
        json!({
            "type": "dispatch",
            "client_event_id": EventId::new().as_uuid().to_string(),
            "command": command,
        })
        .to_string()
        .into(),
    )
}

fn create_project_frame(title: &str) -> Message {
    dispatch_frame(json!({ "type": "create_project", "title": title }))
}

fn create_task_frame(title: &str) -> Message {
    dispatch_frame(json!({ "type": "create_task", "task": { "title": title } }))
}

/// Everything the server wrote until it closed the socket.
struct Closed {
    frames: Vec<Value>,
    close_code: Option<u16>,
}

impl Closed {
    fn assert_refused(&self, code: &str, close_code: u16) {
        assert_eq!(
            self.close_code,
            Some(close_code),
            "wrong close, frames {:?}",
            self.frames
        );
        for frame in &self.frames {
            assert_eq!(frame["type"], "error", "frame leaked: {frame}");
            assert_eq!(frame["code"], code, "frame leaked: {frame}");
        }
    }

    fn assert_revoked(&self) {
        self.assert_refused("auth_revoked", 1008);
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

async fn project_exists(app: &TestApp, title: &str) -> bool {
    app.state
        .projects
        .list_all()
        .await
        .unwrap()
        .iter()
        .any(|project| project.title == title)
}

/// Runs a command as the admin and returns once the server applied it, without
/// going through any socket under test.
async fn admin_creates_task(addr: SocketAddr, app: &TestApp, title: &str) {
    let (mut sink, mut stream) = connect(addr, &app.admin_token).await;
    next_of_type(&mut stream, "hello").await;
    sink.send(create_task_frame(title)).await.unwrap();
    next_of_type(&mut stream, "ack").await;
}

// ── An open socket after DELETE ───────────────────────────────────────────────

#[tokio::test]
async fn command_sent_after_delete_returned_204_has_no_side_effect() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    let addr = serve(app.state.clone()).await;

    let (mut sink, mut stream) = subscribed(addr, &token.plaintext).await;
    delete_token(addr, &app, token.id).await;
    // Either the frame is refused on ingress or the socket is already gone.
    let _ = sink.send(create_project_frame("after delete")).await;

    let closed = read_until_closed(&mut stream).await;
    closed.assert_revoked();
    assert_eq!(closed.frames.len(), 1, "frames: {:?}", closed.frames);
    assert!(!project_exists(&app, "after delete").await);
}

/// `select!` polls its branches in random order, so every round is a coin flip
/// for a main loop that does not check on ingress.
#[tokio::test]
async fn buffered_command_never_beats_a_pending_revocation() {
    let app = test_app().await;
    let addr = serve(app.state.clone()).await;

    for round in 0..32 {
        let token = mint(&app, None).await;
        let (mut sink, mut stream) = connect(addr, &token.plaintext).await;
        next_of_type(&mut stream, "hello").await;
        delete_token(addr, &app, token.id).await;
        let title = format!("buffered {round}");
        let _ = sink.send(create_project_frame(&title)).await;
        read_until_closed(&mut stream).await.assert_revoked();
        assert!(!project_exists(&app, &title).await, "{title} ran");
    }
}

#[tokio::test]
async fn delete_stops_the_live_forwarder() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    let addr = serve(app.state.clone()).await;

    let (_sink, mut stream) = subscribed(addr, &token.plaintext).await;
    admin_creates_task(addr, &app, "before delete").await;
    next_of_type(&mut stream, "event").await;

    delete_token(addr, &app, token.id).await;
    admin_creates_task(addr, &app, "after delete").await;

    let closed = read_until_closed(&mut stream).await;
    closed.assert_revoked();
}

/// No client frame, no event, and the next `Ping` is minutes of test time away:
/// only the hub notification can end this socket.
#[tokio::test]
async fn delete_closes_a_quiet_socket() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    let addr = serve(app.state.clone()).await;

    let (_sink, mut stream) = subscribed(addr, &token.plaintext).await;
    delete_token(addr, &app, token.id).await;

    read_until_closed(&mut stream).await.assert_revoked();
}

#[tokio::test]
async fn frames_queued_behind_the_writer_are_dropped() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    let server = serve_instrumented(&app, &token, Barrier::idle(), Barrier::idle()).await;
    let addr = server.addr;

    let (_sink, mut stream) = subscribed(addr, &token.plaintext).await;
    // The writer parks on the first event, the second queues behind it.
    server.before_get.arm(1);
    admin_creates_task(addr, &app, "held by the writer").await;
    server.before_get.wait_reached().await;
    admin_creates_task(addr, &app, "queued behind it").await;

    delete_token(addr, &app, token.id).await;
    server.before_get.release();

    read_until_closed(&mut stream).await.assert_revoked();
}

/// The barrier sits ahead of the row read, so the revoke commits before this
/// frame clears the writer's check and Hello never goes out.
#[tokio::test]
async fn hello_held_past_the_admission_check_is_replaced_by_the_refusal() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    // Call 1 is the admission check, call 2 the writer holding Hello.
    let server = serve_instrumented(&app, &token, Barrier::armed(2), Barrier::idle()).await;

    let (mut sink, mut stream) = connect(server.addr, &token.plaintext).await;
    server.before_get.wait_reached().await;
    sink.send(subscribe_frame()).await.unwrap();
    sink.send(create_project_frame("sent before hello"))
        .await
        .unwrap();

    delete_token(server.addr, &app, token.id).await;
    server.before_get.release();

    read_until_closed(&mut stream).await.assert_revoked();
    assert!(!project_exists(&app, "sent before hello").await);
}

// ── A revoke the hub never saw ────────────────────────────────────────────────

#[tokio::test]
async fn unnotified_revoke_refuses_the_next_command() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    let addr = serve(app.state.clone()).await;

    let (mut sink, mut stream) = subscribed(addr, &token.plaintext).await;
    app.state.auth_store.revoke(token.id).await.unwrap();
    assert!(!app.state.hub.is_token_revoked(token.id));

    sink.send(create_project_frame("no notification"))
        .await
        .unwrap();

    read_until_closed(&mut stream).await.assert_revoked();
    assert!(!project_exists(&app, "no notification").await);
}

#[tokio::test]
async fn unnotified_revoke_stops_the_next_event() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    let addr = serve(app.state.clone()).await;

    let (_sink, mut stream) = subscribed(addr, &token.plaintext).await;
    app.state.auth_store.revoke(token.id).await.unwrap();
    admin_creates_task(addr, &app, "not for the revoked").await;

    read_until_closed(&mut stream).await.assert_revoked();
}

/// Nothing is latched for an unreadable row, and the client never reads the
/// close, so it never answers: the read loop has to end on the writer's exit.
#[tokio::test]
async fn writer_refusal_ends_the_connection_without_the_client() {
    let app = test_app().await;
    let device = app
        .state
        .devices
        .insert(DeviceId::new(), "test device")
        .await
        .unwrap();
    let token = mint(&app, Some(device.id)).await;
    let server = serve_instrumented(&app, &token, Barrier::idle(), Barrier::idle()).await;

    let (_sink, _stream) = subscribed(server.addr, &token.plaintext).await;
    server.fail_get.store(true, Ordering::SeqCst);
    admin_creates_task(server.addr, &app, "wakes the writer").await;

    let deadline = tokio::time::Instant::now() + DEADLINE;
    while !app.state.hub.connected_devices().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "read loop outlived the refused socket"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn revocation_lost_to_broadcast_lag_still_closes_a_quiet_socket() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    let server = serve_instrumented(&app, &token, Barrier::armed(2), Barrier::idle()).await;

    let (_sink, mut stream) = connect(server.addr, &token.plaintext).await;
    server.before_get.wait_reached().await;

    delete_token(server.addr, &app, token.id).await;
    for _ in 0..256 {
        app.state.hub.notify_token_revoked(TokenId::new());
    }
    server.before_get.release();

    read_until_closed(&mut stream).await.assert_revoked();
}

#[tokio::test]
async fn unreadable_token_row_closes_without_running_the_command() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    let server = serve_instrumented(&app, &token, Barrier::idle(), Barrier::idle()).await;

    let (mut sink, mut stream) = subscribed(server.addr, &token.plaintext).await;
    server.fail_get.store(true, Ordering::SeqCst);
    sink.send(create_project_frame("unverified")).await.unwrap();

    read_until_closed(&mut stream)
        .await
        .assert_refused("auth_storage", 1011);
    assert!(!project_exists(&app, "unverified").await);
    assert!(!app.state.hub.is_token_revoked(token.id));

    // The token itself is fine: reconnecting works once the row is readable.
    server.fail_get.store(false, Ordering::SeqCst);
    let _socket = subscribed(server.addr, &token.plaintext).await;
}

// ── New connections ───────────────────────────────────────────────────────────

#[tokio::test]
async fn reconnect_with_a_revoked_bearer_is_rejected() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    let addr = serve(app.state.clone()).await;

    delete_token(addr, &app, token.id).await;

    let (_sink, mut stream) = connect(addr, &token.plaintext).await;
    let closed = read_until_closed(&mut stream).await;
    assert_eq!(closed.close_code, Some(1008));
    assert!(closed.frames.is_empty(), "frames: {:?}", closed.frames);
}

#[tokio::test]
async fn delete_during_authentication_refuses_the_connection() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    let server = serve_instrumented(&app, &token, Barrier::idle(), Barrier::armed(1)).await;

    let (mut sink, mut stream) = connect(server.addr, &token.plaintext).await;
    server.after_verify.wait_reached().await;
    sink.send(create_project_frame("during auth"))
        .await
        .unwrap();

    delete_token(server.addr, &app, token.id).await;
    server.after_verify.release();

    read_until_closed(&mut stream).await.assert_revoked();
    assert!(!project_exists(&app, "during auth").await);
}

#[tokio::test]
async fn unnotified_revoke_during_authentication_refuses_the_connection() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    let server = serve_instrumented(&app, &token, Barrier::idle(), Barrier::armed(1)).await;

    let (_sink, mut stream) = connect(server.addr, &token.plaintext).await;
    server.after_verify.wait_reached().await;

    app.state.auth_store.revoke(token.id).await.unwrap();
    server.after_verify.release();

    read_until_closed(&mut stream).await.assert_revoked();
}

// ── Only that token ───────────────────────────────────────────────────────────

#[tokio::test]
async fn other_token_of_the_agent_stays_connected() {
    let app = test_app().await;
    let revoked = mint(&app, None).await;
    let mut sibling = generate(NewTokenSpec {
        kind: TokenKind::Pat,
        agent_id: revoked.agent_id,
        scope: TokenScope::default_user(),
        rate_limit_per_min: 600,
        expired_at: None,
    })
    .unwrap();
    sibling.record.device_id = None;
    app.state
        .auth_store
        .insert(sibling.record.clone())
        .await
        .unwrap();
    let addr = serve(app.state.clone()).await;

    let (_revoked_sink, mut revoked_stream) = subscribed(addr, &revoked.plaintext).await;
    let (mut sibling_sink, mut sibling_stream) = subscribed(addr, &sibling.plaintext).await;

    delete_token(addr, &app, revoked.id).await;
    read_until_closed(&mut revoked_stream)
        .await
        .assert_revoked();

    sibling_sink
        .send(create_project_frame("sibling still works"))
        .await
        .unwrap();
    next_of_type(&mut sibling_stream, "ack").await;
    assert!(project_exists(&app, "sibling still works").await);
}

#[tokio::test]
async fn deleting_a_device_token_leaves_the_device_and_its_other_token() {
    let app = test_app().await;
    let device = app
        .state
        .devices
        .insert(DeviceId::new(), "test device")
        .await
        .unwrap();
    let revoked = mint(&app, Some(device.id)).await;
    let other = mint(&app, Some(device.id)).await;
    let addr = serve(app.state.clone()).await;

    let (_revoked_sink, mut revoked_stream) = subscribed(addr, &revoked.plaintext).await;
    let (mut other_sink, mut other_stream) = subscribed(addr, &other.plaintext).await;

    delete_token(addr, &app, revoked.id).await;
    read_until_closed(&mut revoked_stream)
        .await
        .assert_revoked();

    assert!(!app.state.hub.is_device_revoked(device.id));
    other_sink
        .send(create_project_frame("same device still works"))
        .await
        .unwrap();
    next_of_type(&mut other_stream, "ack").await;
}

/// Each server or tenant has its own store and hub: the same token id elsewhere
/// is not this revoke's business.
#[tokio::test]
async fn revoke_on_one_server_leaves_another() {
    let app = test_app().await;
    let elsewhere = test_app().await;
    let token = mint(&app, None).await;
    elsewhere
        .state
        .auth_store
        .insert(token.record.clone())
        .await
        .unwrap();
    let addr = serve(app.state.clone()).await;
    let elsewhere_addr = serve(elsewhere.state.clone()).await;

    let (_sink, mut stream) = subscribed(addr, &token.plaintext).await;
    let (mut other_sink, mut other_stream) = subscribed(elsewhere_addr, &token.plaintext).await;

    delete_token(addr, &app, token.id).await;
    read_until_closed(&mut stream).await.assert_revoked();

    other_sink
        .send(create_project_frame("other server still works"))
        .await
        .unwrap();
    next_of_type(&mut other_stream, "ack").await;
    assert!(project_exists(&elsewhere, "other server still works").await);
}

#[tokio::test]
async fn project_scoped_token_stays_refused() {
    let app = test_app().await;
    let mut scope = TokenScope::default_user();
    scope.projects = ProjectFilter::Only {
        projects: vec![ProjectId::new()],
    };
    assert!(scope.capabilities.has(Capability::TaskWrite));
    let secret = generate(NewTokenSpec {
        kind: TokenKind::Pat,
        agent_id: AgentId::new(),
        scope,
        rate_limit_per_min: 60,
        expired_at: None,
    })
    .unwrap();
    app.state.auth_store.insert(secret.record).await.unwrap();
    let addr = serve(app.state.clone()).await;

    let (mut sink, mut stream) = connect(addr, &secret.plaintext).await;
    let _ = sink.send(create_task_frame("project scoped")).await;

    let closed = read_until_closed(&mut stream).await;
    assert!(closed.frames.is_empty(), "frames: {:?}", closed.frames);
    assert!(!task_exists(&app, "project scoped").await);
}

#[tokio::test]
async fn capability_gate_survives_the_token_check() {
    let app = test_app().await;
    let secret = generate(NewTokenSpec {
        kind: TokenKind::Pat,
        agent_id: AgentId::new(),
        scope: TokenScope {
            projects: ProjectFilter::All,
            capabilities: Capabilities::from_bits(Capability::TaskRead.as_bit()),
        },
        rate_limit_per_min: 60,
        expired_at: None,
    })
    .unwrap();
    app.state.auth_store.insert(secret.record).await.unwrap();
    let addr = serve(app.state.clone()).await;

    let (mut sink, mut stream) = connect(addr, &secret.plaintext).await;
    next_of_type(&mut stream, "hello").await;
    sink.send(create_project_frame("no capability"))
        .await
        .unwrap();

    let error = next_of_type(&mut stream, "error").await;
    assert_eq!(error["code"], "forbidden", "error: {error}");
    assert!(!project_exists(&app, "no capability").await);
}

// ── HTTP cache ────────────────────────────────────────────────────────────────

async fn list_projects(addr: SocketAddr, authorization: &str) -> (u16, Value) {
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/v1/projects"))
        .header("authorization", authorization)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

fn assert_revoked_401(reply: (u16, Value)) {
    assert_eq!(reply.0, 401, "reply: {:?}", reply.1);
    assert_eq!(
        reply.1["error"]["code"], "auth_revoked",
        "reply: {:?}",
        reply.1
    );
}

#[tokio::test]
async fn delete_refuses_a_cached_bearer_at_once() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    let addr = serve(app.state.clone()).await;
    let bearer = format!("Bearer {}", token.plaintext);

    assert_eq!(list_projects(addr, &bearer).await.0, 200);
    delete_token(addr, &app, token.id).await;

    assert_revoked_401(list_projects(addr, &bearer).await);
}

#[tokio::test]
async fn unnotified_revoke_keeps_the_cached_bearer_until_its_entry_goes() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    let addr = serve(app.state.clone()).await;
    let cached = format!("Bearer {}", token.plaintext);
    // Another spelling of the same bearer is a different cache key.
    let uncached = format!("bearer {}", token.plaintext);

    assert_eq!(list_projects(addr, &cached).await.0, 200);
    app.state.auth_store.revoke(token.id).await.unwrap();

    assert_eq!(list_projects(addr, &cached).await.0, 200);
    assert_revoked_401(list_projects(addr, &uncached).await);
}

#[tokio::test]
async fn revoke_found_by_a_socket_also_ends_the_cached_bearer() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    let addr = serve(app.state.clone()).await;
    let bearer = format!("Bearer {}", token.plaintext);

    let (mut sink, mut stream) = subscribed(addr, &token.plaintext).await;
    assert_eq!(list_projects(addr, &bearer).await.0, 200);
    app.state.auth_store.revoke(token.id).await.unwrap();
    sink.send(create_project_frame("found by the socket"))
        .await
        .unwrap();
    read_until_closed(&mut stream).await.assert_revoked();

    assert_revoked_401(list_projects(addr, &bearer).await);
}

/// The verification read the row before the revoke and finishes after it: it
/// must neither pass the request nor leave a live cache entry behind.
#[tokio::test]
async fn in_flight_verification_cannot_outlive_delete() {
    let app = test_app().await;
    let token = mint(&app, None).await;
    let server = serve_instrumented(&app, &token, Barrier::idle(), Barrier::armed(1)).await;
    let addr = server.addr;
    let bearer = format!("Bearer {}", token.plaintext);

    let in_flight = {
        let bearer = bearer.clone();
        tokio::spawn(async move { list_projects(addr, &bearer).await })
    };
    server.after_verify.wait_reached().await;

    delete_token(addr, &app, token.id).await;
    server.after_verify.release();

    assert_revoked_401(in_flight.await.unwrap());
    assert_revoked_401(list_projects(addr, &bearer).await);
}
