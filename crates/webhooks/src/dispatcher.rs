//! Bus-driven webhook dispatcher.
//!
//! `spawn_dispatcher` returns a [`DispatcherHandle`] that owns a tokio
//! task subscribed to the in-process `EventBus`. For every published
//! envelope the dispatcher:
//!   1. loads the *active* webhook list,
//!   2. filters by `events_mask` (kind allow-list) and `project_filter`,
//!   3. resolves each subscription's `enrich` keys against the supplied
//!      [`EnrichmentSource`] (§3.7.5), folding the result into the outbound
//!      JSON as a top-level `context` field,
//!   4. POSTs the JSON payload with `X-Taskagent-Signature`.
//!
//! Delivery is single-shot for the MVP — there is no retry loop and no
//! delivery queue. Failures are logged and recorded in
//! `webhook_deliveries`; retries land in a later wave.
//!
//! Per-recipient body serialisation: the envelope is serialised once and
//! re-used when `enrich` is empty; subscriptions that opt-in to context
//! get a fresh per-recipient body so each receiver sees only its own
//! requested keys.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use taskagent_auth::ProjectFilter;
use taskagent_events::{EventEnvelope, EventReceiver};
use taskagent_shared::{ProjectId, WebhookId};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::enrich::{build_context, EnrichmentSource};
use crate::model::Webhook;
use crate::sign::sign_body_hex;
use crate::store::WebhookStore;

/// Lifetime handle for the spawned dispatcher task. Drop to abort.
pub struct DispatcherHandle {
    task: JoinHandle<()>,
}

impl DispatcherHandle {
    pub fn abort(&self) {
        self.task.abort();
    }
}

impl Drop for DispatcherHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Spawn the dispatcher task. The caller supplies a fresh `EventReceiver`
/// (typically `hub.subscribe()`) plus the store, a reusable HTTP client,
/// and an [`EnrichmentSource`]. Pass `Arc::new(NoopEnrichment)` to disable
/// enrichment entirely (existing behaviour).
pub fn spawn_dispatcher(
    mut receiver: EventReceiver,
    store: Arc<dyn WebhookStore>,
    http: reqwest::Client,
    enrichment: Arc<dyn EnrichmentSource>,
) -> DispatcherHandle {
    let task = tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(env) => {
                    if let Err(e) = dispatch_one(&store, &http, &enrichment, &env).await {
                        tracing::warn!(error = %e, "webhook dispatch failed");
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "webhook dispatcher lagged");
                    continue;
                }
                Err(RecvError::Closed) => {
                    tracing::info!("webhook dispatcher: bus closed, exiting");
                    return;
                }
            }
        }
    });

    DispatcherHandle { task }
}

async fn dispatch_one(
    store: &Arc<dyn WebhookStore>,
    http: &reqwest::Client,
    enrichment: &Arc<dyn EnrichmentSource>,
    env: &EventEnvelope,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let kind = env.kind();
    let webhooks = store.list_active().await?;
    if webhooks.is_empty() {
        return Ok(());
    }

    // The unenriched body is shared across every recipient that opted out
    // of enrichment — we still pay a single serialisation cost on a hot
    // bus event regardless of subscriber count.
    let bare_body = serde_json::to_vec(env)?;
    let target_project = env.payload.target_project();

    for w in webhooks {
        if !w.matches_kind(kind) {
            continue;
        }
        if !project_matches(&w.project_filter, target_project) {
            continue;
        }

        let body = if w.enrich.is_empty() {
            // Fast path: identical bytes for every plain subscriber.
            bare_body.clone()
        } else {
            // Slow path: re-serialise via Value so we can inject `context`
            // without disturbing the existing envelope shape.
            match build_enriched_body(env, &w.enrich, enrichment).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!(error = %e, webhook_id = %w.id, "enrichment failed; falling back to bare envelope");
                    bare_body.clone()
                }
            }
        };

        let http_clone = http.clone();
        let store_clone = Arc::clone(store);
        let kind_owned = kind.to_string();
        let env_id = env.id;
        tokio::spawn(async move {
            send(&http_clone, &store_clone, &w, kind_owned, env_id, body).await;
        });
    }

    Ok(())
}

/// Serialise the envelope, ask the [`EnrichmentSource`] for every requested
/// key, and merge the resolved map into a top-level `context` field. If no
/// key resolves (or `enrich` is empty), the byte output is identical to a
/// plain `serde_json::to_vec(env)` so pre-§3.7.5 subscribers keep the exact
/// same wire format they had before.
async fn build_enriched_body(
    env: &EventEnvelope,
    enrich: &[String],
    enrichment: &Arc<dyn EnrichmentSource>,
) -> Result<Vec<u8>, serde_json::Error> {
    let Some(context) = build_context(enrich, env, enrichment).await else {
        return serde_json::to_vec(env);
    };
    let mut as_value = serde_json::to_value(env)?;
    if let Value::Object(map) = &mut as_value {
        // BTreeMap → Value::Object preserves key order in serde_json.
        let ctx_val = serde_json::to_value(context)?;
        map.insert("context".to_string(), ctx_val);
    }
    serde_json::to_vec(&as_value)
}

fn project_matches(filter: &ProjectFilter, project_id: Option<ProjectId>) -> bool {
    filter.allows(project_id)
}

async fn send(
    http: &reqwest::Client,
    store: &Arc<dyn WebhookStore>,
    webhook: &Webhook,
    event_kind: String,
    event_id: taskagent_shared::EventId,
    body: Vec<u8>,
) {
    let signature = sign_body_hex(&webhook.secret, &body);
    let delivery_id = uuid::Uuid::now_v7().to_string();
    let user_agent = format!("taskagent/{}", env!("CARGO_PKG_VERSION"));

    let res = http
        .post(&webhook.url)
        .header("content-type", "application/json")
        .header("user-agent", user_agent)
        .header("x-taskagent-delivery", &delivery_id)
        .header("x-taskagent-event", &event_kind)
        .header("x-taskagent-signature", &signature)
        .body(body)
        .timeout(Duration::from_secs(5))
        .send()
        .await;

    let (status_code, succeeded, error) = match res {
        Ok(resp) => {
            let code = resp.status().as_u16();
            let ok = resp.status().is_success();
            (
                Some(code),
                ok,
                if ok {
                    None
                } else {
                    Some(format!("HTTP {code}"))
                },
            )
        }
        Err(e) => (None, false, Some(e.to_string())),
    };

    let _ = log_delivery(
        store,
        webhook.id,
        event_id,
        &event_kind,
        status_code,
        succeeded,
        1,
        error.as_deref(),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn log_delivery(
    store: &Arc<dyn WebhookStore>,
    webhook_id: WebhookId,
    event_id: taskagent_shared::EventId,
    event_kind: &str,
    status_code: Option<u16>,
    succeeded: bool,
    attempts: u32,
    error: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    store
        .record_delivery(
            webhook_id,
            event_id,
            event_kind,
            status_code,
            succeeded,
            attempts,
            error,
        )
        .await?;
    Ok(())
}
