//! Webhook payload enrichment (§3.7.5 / LIN B.4).
//!
//! Before POSTing an envelope the dispatcher consults each subscription's
//! `enrich` list and asks an [`EnrichmentSource`] for additional context.
//! The resulting key/value pairs are folded into the outbound JSON under a
//! top-level `context` field.
//!
//! The trait lives in this crate rather than in `daruma-storage` because:
//!   * the dispatcher must not depend on storage (storage already depends on
//!     this crate via `impl WebhookStore for WebhookRepo`);
//!   * the contract is small and stable — `apps/server` wires the concrete
//!     storage-backed implementation at startup.
//!
//! Unknown keys are silently skipped (logged at `warn`) so a future server
//! can introduce a new key without breaking old subscribers and a stale
//! client can opt-in to keys before the server learns them.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use daruma_events::EventEnvelope;

/// Reserved enrich keys recognised by the built-in
/// [`EnrichmentSource`] implementation. Anything else is logged + skipped.
pub mod keys {
    /// The first plan that contains the target task. Value shape:
    /// `{ "id": "...", "title": "...", "status": "draft"|"active"|... }`.
    pub const PARENT_PLAN: &str = "parent_plan";

    /// The owning project of the event's target task or plan. Value shape:
    /// `{ "id": "...", "title": "..." }`.
    pub const PROJECT: &str = "project";

    /// The target task projection. Value shape:
    /// `{ "id": "...", "title": "...", "status": "...", "priority": "..." }`.
    pub const TASK: &str = "task";
}

/// Pluggable context source. Implementations are expected to be cheap
/// fan-out queries against the projection tables; the dispatcher awaits
/// them inline before POSTing, so each method should bound its own work.
///
/// All methods return `Option` so a key that does not apply to the given
/// envelope (e.g. `parent_plan` for a `ProjectCreated`) collapses silently
/// to "no entry in `context`" instead of an error.
#[async_trait]
pub trait EnrichmentSource: Send + Sync + 'static {
    /// Resolve a single enrich key. Implementations may inspect the
    /// envelope (`payload.target_task()` / `target_project()`) to decide
    /// what to do. Returning `Ok(None)` means "key does not apply" and is
    /// indistinguishable from "key not requested" for the recipient.
    async fn resolve(&self, key: &str, env: &EventEnvelope) -> Option<Value>;
}

/// Default zero-config implementation that produces no context. Useful
/// for tests, integration suites that don't exercise enrichment, and as
/// a kill-switch when wiring is in flux.
#[derive(Clone, Copy, Default)]
pub struct NoopEnrichment;

#[async_trait]
impl EnrichmentSource for NoopEnrichment {
    async fn resolve(&self, _key: &str, _env: &EventEnvelope) -> Option<Value> {
        None
    }
}

/// Build the `context` object for a single delivery by resolving every
/// requested key against `source`. Unknown keys (those `source` doesn't
/// recognise *and* doesn't apply to this envelope) are dropped silently
/// — the dispatcher emits a single `warn!` at delivery time so log
/// volume stays bounded.
///
/// Returns `None` when `enrich` is empty *or* every key resolved to
/// `None`; callers should skip injecting an empty `context` field in
/// that case to keep the wire format identical to pre-§3.7.5
/// deliveries.
pub async fn build_context(
    enrich: &[String],
    env: &EventEnvelope,
    source: &Arc<dyn EnrichmentSource>,
) -> Option<BTreeMap<String, Value>> {
    if enrich.is_empty() {
        return None;
    }
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    for key in enrich {
        match source.resolve(key, env).await {
            Some(v) => {
                out.insert(key.clone(), v);
            }
            None => {
                // Quiet by default — a key that doesn't apply to this event
                // is a normal occurrence (e.g. `task` on a `ProjectCreated`).
                // We only debug-log so operators can audit by enabling
                // `daruma_webhooks=debug` if a subscriber is missing
                // expected context.
                tracing::debug!(
                    key = %key,
                    event_kind = env.kind(),
                    "enrich key did not apply to this envelope"
                );
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use daruma_domain::Actor;
    use daruma_events::Event;
    use daruma_shared::{ProjectId, TaskId};

    #[derive(Default)]
    struct StubSource;

    #[async_trait]
    impl EnrichmentSource for StubSource {
        async fn resolve(&self, key: &str, _env: &EventEnvelope) -> Option<Value> {
            match key {
                "project" => Some(json!({"id": "p", "title": "Stub"})),
                _ => None,
            }
        }
    }

    fn env() -> EventEnvelope {
        EventEnvelope::new(
            Actor::user(),
            Event::ProjectUpdated {
                project_id: ProjectId::new(),
                title: Some("x".into()),
                description: None,
            },
        )
    }

    #[tokio::test]
    async fn empty_enrich_returns_none() {
        let src: Arc<dyn EnrichmentSource> = Arc::new(StubSource);
        let env = env();
        assert!(build_context(&[], &env, &src).await.is_none());
    }

    #[tokio::test]
    async fn unknown_keys_are_dropped() {
        let src: Arc<dyn EnrichmentSource> = Arc::new(StubSource);
        let env = env();
        let ctx = build_context(&["nonexistent".to_string()], &env, &src).await;
        // every requested key resolved to None ⇒ ctx is None
        assert!(ctx.is_none());
    }

    #[tokio::test]
    async fn known_key_is_included() {
        let src: Arc<dyn EnrichmentSource> = Arc::new(StubSource);
        let env = env();
        let ctx = build_context(&["project".to_string()], &env, &src)
            .await
            .expect("context");
        assert_eq!(ctx.get("project").unwrap()["title"], "Stub");
    }

    #[tokio::test]
    async fn mixed_keys_partial_resolution() {
        let src: Arc<dyn EnrichmentSource> = Arc::new(StubSource);
        let env = env();
        let ctx = build_context(&["project".to_string(), "unknown".to_string()], &env, &src)
            .await
            .expect("context");
        assert!(ctx.contains_key("project"));
        assert!(!ctx.contains_key("unknown"));
    }

    // touch unused symbols so dead_code does not fire when this module is
    // pulled into the production build.
    #[test]
    fn task_id_is_used() {
        let _ = TaskId::new();
    }
}
