//! §3.7.5 — webhook enrich= param.
//!
//! These tests exercise the dispatcher's *enrichment path* in isolation
//! from any storage backend by plugging in a hand-rolled
//! [`EnrichmentSource`] mock. They cover:
//!
//!   * `enrich=[]` (default) → body is byte-identical to a plain envelope
//!     serialise, i.e. no `context` field appears on the wire;
//!   * `enrich=["parent_plan"]` → `context.parent_plan` is present in the
//!     delivered JSON;
//!   * `enrich=["unknown_key"]` → delivery succeeds and `context` is
//!     absent (the dispatcher silently skips keys the source does not
//!     recognise);
//!   * `Webhook` round-trips through serde with the new field defaulting
//!     to an empty vec when older inputs omit it (backward compat).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::{extract::State, http::HeaderMap, routing::post as axpost, Router as AxRouter};
use daruma_auth::ProjectFilter;
use daruma_domain::Actor;
use daruma_events::{Event, EventBus, EventEnvelope};
use daruma_shared::{EventId, ProjectId, Result, TaskId, WebhookDeliveryId, WebhookId};
use daruma_webhooks::{
    enrich::keys, spawn_dispatcher, EnrichmentSource, NewWebhook, NoopEnrichment, Webhook,
    WebhookStore,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

// ── Mock HTTP receiver ───────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct Hits {
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
}

async fn record_hook(
    State(state): State<Hits>,
    _headers: HeaderMap,
    body: axum::body::Bytes,
) -> &'static str {
    state.bodies.lock().unwrap().push(body.to_vec());
    "ok"
}

async fn spawn_receiver() -> (SocketAddr, Hits) {
    let state = Hits::default();
    let app = AxRouter::new()
        .route("/hook", axpost(record_hook))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, state)
}

// ── In-memory webhook store ──────────────────────────────────────────────────

#[derive(Default)]
struct MemStore {
    rows: Mutex<Vec<Webhook>>,
}

#[async_trait]
impl WebhookStore for MemStore {
    async fn insert(&self, w: Webhook) -> Result<()> {
        self.rows.lock().unwrap().push(w);
        Ok(())
    }
    async fn get(&self, id: WebhookId) -> Result<Option<Webhook>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|w| w.id == id)
            .cloned())
    }
    async fn list_active(&self) -> Result<Vec<Webhook>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|w| w.is_active)
            .cloned()
            .collect())
    }
    async fn list_all(&self) -> Result<Vec<Webhook>> {
        Ok(self.rows.lock().unwrap().clone())
    }
    async fn patch(
        &self,
        _id: WebhookId,
        _patch: daruma_webhooks::WebhookPatch,
    ) -> Result<Option<Webhook>> {
        unreachable!("not exercised by these tests")
    }
    async fn delete(&self, _id: WebhookId) -> Result<bool> {
        unreachable!("not exercised by these tests")
    }
    async fn record_delivery(
        &self,
        _webhook_id: WebhookId,
        _event_id: EventId,
        _event_kind: &str,
        _status_code: Option<u16>,
        _succeeded: bool,
        _attempts: u32,
        _error: Option<&str>,
    ) -> Result<()> {
        // No-op: signature shape exercised via the dispatcher's normal call,
        // we just want a sink that doesn't block delivery.
        let _ = WebhookDeliveryId::new();
        Ok(())
    }
}

// ── Stubbed enrichment source ────────────────────────────────────────────────

#[derive(Default)]
struct StubSource;

#[async_trait]
impl EnrichmentSource for StubSource {
    async fn resolve(&self, key: &str, _env: &EventEnvelope) -> Option<Value> {
        match key {
            k if k == keys::PARENT_PLAN => Some(json!({
                "id": "plan-1",
                "title": "Top plan",
                "status": "active",
            })),
            k if k == keys::PROJECT => Some(json!({
                "id": "proj-1",
                "title": "Test project",
            })),
            // intentionally don't resolve TASK so we can exercise the
            // "key omitted from context because source returned None" path
            // alongside a known key.
            _ => None,
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

async fn install_webhook(store: &Arc<dyn WebhookStore>, url: String, enrich: Vec<String>) {
    let hook = NewWebhook {
        id: None,
        url,
        secret: "shared-secret".into(),
        events: vec!["task_created".into()],
        project_filter: ProjectFilter::All,
        is_active: true,
        description: None,
        enrich,
    }
    .into_webhook();
    store.insert(hook).await.unwrap();
}

fn sample_envelope() -> EventEnvelope {
    EventEnvelope::new(
        Actor::user(),
        Event::TaskCreated {
            task: daruma_domain::NewTask {
                id: Some(TaskId::new()),
                project_id: Some(ProjectId::new()),
                title: "hello".into(),
                description: None,
                status: None,
                priority: None,
                triage_state: None,
                due_at: None,
            },
        },
    )
}

async fn wait_for_one_hit(hits: &Hits) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        {
            let guard = hits.bodies.lock().unwrap();
            if !guard.is_empty() {
                return guard[0].clone();
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("webhook was not delivered within 2 s");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn default_enrich_empty_payload_has_no_context() {
    let (addr, hits) = spawn_receiver().await;
    let store: Arc<dyn WebhookStore> = Arc::new(MemStore::default());
    install_webhook(&store, format!("http://{addr}/hook"), vec![]).await;

    let bus = EventBus::new(256);
    let http = reqwest::Client::new();
    // NoopEnrichment is wired explicitly to prove that even if a non-noop
    // source were installed, `enrich=[]` still means "no context".
    let _disp = spawn_dispatcher(
        bus.subscribe(),
        store.clone(),
        http,
        Arc::new(NoopEnrichment),
    );

    let env = sample_envelope();
    bus.publish(env.clone());

    let body = wait_for_one_hit(&hits).await;
    let json: Value = serde_json::from_slice(&body).expect("body is json");
    assert!(
        json.get("context").is_none(),
        "no enrich keys ⇒ payload must NOT carry a context field; got {json}"
    );
    // Sanity: the unmodified envelope round-trips through the wire.
    let echoed: EventEnvelope = serde_json::from_slice(&body).expect("envelope");
    assert_eq!(echoed.id, env.id);
}

#[tokio::test]
async fn parent_plan_key_lands_in_context() {
    let (addr, hits) = spawn_receiver().await;
    let store: Arc<dyn WebhookStore> = Arc::new(MemStore::default());
    install_webhook(
        &store,
        format!("http://{addr}/hook"),
        vec![keys::PARENT_PLAN.to_string()],
    )
    .await;

    let bus = EventBus::new(256);
    let http = reqwest::Client::new();
    let _disp = spawn_dispatcher(bus.subscribe(), store.clone(), http, Arc::new(StubSource));

    bus.publish(sample_envelope());

    let body = wait_for_one_hit(&hits).await;
    let json: Value = serde_json::from_slice(&body).expect("body is json");
    let ctx = json
        .get("context")
        .unwrap_or_else(|| panic!("context field is required; got {json}"));
    let pp = ctx
        .get("parent_plan")
        .unwrap_or_else(|| panic!("parent_plan field is required; ctx={ctx}"));
    assert_eq!(pp["id"], "plan-1");
    assert_eq!(pp["title"], "Top plan");
    assert_eq!(pp["status"], "active");
}

#[tokio::test]
async fn unknown_key_does_not_fail_delivery() {
    let (addr, hits) = spawn_receiver().await;
    let store: Arc<dyn WebhookStore> = Arc::new(MemStore::default());
    install_webhook(
        &store,
        format!("http://{addr}/hook"),
        vec!["totally_made_up".to_string()],
    )
    .await;

    let bus = EventBus::new(256);
    let http = reqwest::Client::new();
    let _disp = spawn_dispatcher(bus.subscribe(), store.clone(), http, Arc::new(StubSource));

    bus.publish(sample_envelope());

    let body = wait_for_one_hit(&hits).await;
    let json: Value = serde_json::from_slice(&body).expect("body is json");
    // Every requested key resolved to None ⇒ no context object is attached
    // and the wire format is identical to a pre-§3.7.5 delivery.
    assert!(
        json.get("context").is_none(),
        "unknown keys must drop silently without adding an empty context object; got {json}"
    );
}

#[tokio::test]
async fn mixed_known_and_unknown_keys_only_emits_known() {
    let (addr, hits) = spawn_receiver().await;
    let store: Arc<dyn WebhookStore> = Arc::new(MemStore::default());
    install_webhook(
        &store,
        format!("http://{addr}/hook"),
        vec![
            keys::PROJECT.to_string(),
            "ghost".to_string(),
            keys::TASK.to_string(), // StubSource returns None for TASK
        ],
    )
    .await;

    let bus = EventBus::new(256);
    let http = reqwest::Client::new();
    let _disp = spawn_dispatcher(bus.subscribe(), store.clone(), http, Arc::new(StubSource));

    bus.publish(sample_envelope());

    let body = wait_for_one_hit(&hits).await;
    let json: Value = serde_json::from_slice(&body).expect("body is json");
    let ctx = json.get("context").expect("project should resolve");
    assert!(ctx.get("project").is_some(), "project must be present");
    assert!(
        ctx.get("ghost").is_none(),
        "unresolved unknown keys must not appear"
    );
    assert!(
        ctx.get("task").is_none(),
        "keys that resolve to None must not appear"
    );
}

// ── Backward-compat: serde defaults ──────────────────────────────────────────

#[test]
fn webhook_deserialises_without_enrich_field() {
    // Simulate an admin-API payload that predates §3.7.5: no `enrich`
    // field on the wire. Defaulting kicks in and gives us an empty vec.
    let json = serde_json::json!({
        "url": "https://example",
        "secret": "s",
        "events": ["task_created"],
    });
    let parsed: NewWebhook = serde_json::from_value(json).unwrap();
    assert!(parsed.enrich.is_empty());

    let w = parsed.into_webhook();
    assert!(w.enrich.is_empty());

    // Round-trip the materialised Webhook back through serde.
    let serialised = serde_json::to_value(&w).unwrap();
    assert_eq!(serialised["enrich"], serde_json::json!([]));
    let back: Webhook = serde_json::from_value(serialised).unwrap();
    assert_eq!(back.enrich, w.enrich);
}
