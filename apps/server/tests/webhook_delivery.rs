//! AC-9 — webhook delivery + HMAC signature.
//!
//! Spawns two axum servers: the real `daruma-server` and an in-test
//! "mock receiver" that records every POST it gets. Creates a webhook
//! pointing at the receiver, drives a `CreateTask`, then asserts:
//!   * the receiver got exactly one POST within ~1 s of the event;
//!   * `X-Daruma-Event` matches the event kind;
//!   * `X-Daruma-Signature` is `hex(hmac_sha256(secret, body))`;
//!   * `X-Daruma-Delivery` is present;
//!   * `User-Agent` starts with `daruma/`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{extract::State, http::HeaderMap, routing::post, Router as AxumRouter};
use daruma_domain::{Actor, PlanPatch};
use daruma_events::{Event, EventBus, EventEnvelope};
use daruma_shared::PlanId;
use daruma_webhooks::{sign_body_hex, spawn_dispatcher, NoopEnrichment};
use serde_json::json;
use tokio::net::TcpListener;

mod common;

// ── Mock receiver ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct MockHit {
    headers: HeaderMap,
    body: Vec<u8>,
}

#[derive(Clone, Default)]
struct MockState {
    hits: Arc<Mutex<Vec<MockHit>>>,
}

async fn record_hook(
    State(state): State<MockState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> &'static str {
    state.hits.lock().unwrap().push(MockHit {
        headers: headers.clone(),
        body: body.to_vec(),
    });
    "ok"
}

async fn spawn_mock_receiver() -> (SocketAddr, MockState) {
    let state = MockState::default();
    let app = AxumRouter::new()
        .route("/hook", post(record_hook))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, state)
}

// ── daruma-server harness ─────────────────────────────────────────────────

struct ServerHandle {
    addr: SocketAddr,
    token: String,
    /// Direct access to the event bus — lets tests publish events without
    /// going through the HTTP layer (useful when command routing is not
    /// relevant to the assertion being made).
    bus: EventBus,
    // Keep the dispatcher alive for the duration of the test.
    _dispatcher: daruma_webhooks::DispatcherHandle,
}

async fn spawn_daruma() -> ServerHandle {
    let app = common::test_app().await;

    // Spawn the dispatcher — this is what the test is actually exercising.
    let http = reqwest::Client::new();
    let dispatcher = spawn_dispatcher(
        app.bus.subscribe(),
        app.state.webhook_store.clone(),
        http,
        Arc::new(NoopEnrichment),
    );

    let addr = common::spawn_server(&app).await;
    ServerHandle {
        addr,
        token: app.admin_token,
        bus: app.bus,
        _dispatcher: dispatcher,
    }
}

async fn http_post_json(
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
        .expect("post succeeds");
    let status = resp.status().as_u16();
    let json = resp
        .json::<serde_json::Value>()
        .await
        .unwrap_or(serde_json::Value::Null);
    (status, json)
}

// ── AC-9 ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn ac9_webhook_delivered_with_valid_hmac() {
    // 1. Spawn the mock receiver and the real server (with dispatcher).
    let (mock_addr, mock_state) = spawn_mock_receiver().await;
    let server = spawn_daruma().await;
    let client = reqwest::Client::new();

    let server_url = format!("http://{}", server.addr);
    let mock_url = format!("http://{mock_addr}/hook");
    let secret = "test-secret-for-ac9";

    // 2. Create a webhook subscription via the admin endpoint.
    let (status, body) = http_post_json(
        &client,
        &format!("{server_url}/v1/webhooks"),
        &server.token,
        json!({
            "url": mock_url,
            "secret": secret,
            "events": ["task_created"],
            "is_active": true,
        }),
    )
    .await;
    assert_eq!(status, 201, "POST /v1/webhooks => 201; got body: {body}");

    // 3. Drive an event.
    let (status, _envs) = http_post_json(
        &client,
        &format!("{server_url}/v1/commands"),
        &server.token,
        json!({"command":{"type":"create_task","task":{"title":"AC-9 webhook"}}}),
    )
    .await;
    assert_eq!(status, 200);

    // 4. Wait up to 2 s for the mock to record the POST.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if !mock_state.hits.lock().unwrap().is_empty() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("mock receiver did not get the webhook within 2 s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 5. Verify exactly one hit and its envelope.
    let hits = mock_state.hits.lock().unwrap().clone();
    assert_eq!(hits.len(), 1, "expected exactly one webhook delivery");
    let hit = &hits[0];

    // 5a. Standard headers.
    let event_header = hit
        .headers
        .get("x-daruma-event")
        .expect("X-Daruma-Event present")
        .to_str()
        .unwrap();
    assert_eq!(event_header, "task_created");

    let delivery_header = hit
        .headers
        .get("x-daruma-delivery")
        .expect("X-Daruma-Delivery present")
        .to_str()
        .unwrap();
    assert!(!delivery_header.is_empty());

    let user_agent = hit
        .headers
        .get("user-agent")
        .expect("User-Agent present")
        .to_str()
        .unwrap();
    assert!(user_agent.starts_with("daruma/"));

    // 5b. HMAC signature must match `hex(hmac_sha256(secret, body))`.
    let signature_header = hit
        .headers
        .get("x-daruma-signature")
        .expect("X-Daruma-Signature present")
        .to_str()
        .unwrap();
    let expected = sign_body_hex(secret, &hit.body);
    assert_eq!(signature_header, expected, "HMAC must match");

    // 5c. Body parses as an EventEnvelope with the expected kind.
    let envelope: serde_json::Value = serde_json::from_slice(&hit.body).unwrap();
    assert_eq!(envelope["payload"]["type"], "task_created");
}

// ── W2: plan_updated webhook includes parent_plan_id diff ────────────────────

/// W2 AC: When a `PlanUpdated` event with `parent_plan_id` change is published,
/// the webhook dispatcher fires for a `plan_updated` subscription, and the
/// delivered payload includes the `parent_plan_id` field in the patch.
#[tokio::test]
async fn plan_updated_webhook_includes_parent_plan_id() {
    let (mock_addr, mock_state) = spawn_mock_receiver().await;
    let server = spawn_daruma().await;
    let client = reqwest::Client::new();

    let server_url = format!("http://{}", server.addr);
    let mock_url = format!("http://{mock_addr}/hook");
    let secret = "test-secret-w2-plan";

    // Register a webhook for plan_updated events.
    let (status, body) = http_post_json(
        &client,
        &format!("{server_url}/v1/webhooks"),
        &server.token,
        json!({
            "url": mock_url,
            "secret": secret,
            "events": ["plan_updated"],
            "is_active": true,
        }),
    )
    .await;
    assert_eq!(status, 201, "POST /v1/webhooks => 201; body: {body}");

    // Publish a PlanUpdated event with a parent_plan_id change directly via bus.
    let plan_id = PlanId::new();
    let parent_id = PlanId::new();
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

    // Wait up to 2 s for the mock to record the POST.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if !mock_state.hits.lock().unwrap().is_empty() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("webhook was not delivered within 2 s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let hits = mock_state.hits.lock().unwrap().clone();
    assert_eq!(hits.len(), 1, "expected exactly one webhook delivery");
    let hit = &hits[0];

    // Event header must identify the kind.
    let event_header = hit
        .headers
        .get("x-daruma-event")
        .expect("X-Daruma-Event present")
        .to_str()
        .unwrap();
    assert_eq!(event_header, "plan_updated");

    // HMAC must verify.
    let sig = hit
        .headers
        .get("x-daruma-signature")
        .expect("X-Daruma-Signature present")
        .to_str()
        .unwrap();
    assert_eq!(sig, sign_body_hex(secret, &hit.body), "HMAC must match");

    // Body must contain the parent_plan_id diff.
    let envelope: serde_json::Value = serde_json::from_slice(&hit.body).unwrap();
    assert_eq!(envelope["payload"]["type"], "plan_updated");
    assert_eq!(
        envelope["payload"]["patch"]["parent_plan_id"]
            .as_str()
            .unwrap_or(""),
        parent_id.as_uuid().to_string(),
        "webhook body must include parent_plan_id in the patch"
    );
}
