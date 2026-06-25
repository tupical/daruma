//! Storage trait for webhook persistence.

use async_trait::async_trait;
use daruma_shared::{Result, WebhookId};

use crate::model::{Webhook, WebhookPatch};

/// Persistence API for webhook configuration rows. Cheap to clone; SQLite
/// implementation lives in `daruma-storage::WebhookRepo`.
#[async_trait]
pub trait WebhookStore: Send + Sync + 'static {
    async fn insert(&self, webhook: Webhook) -> Result<()>;

    async fn get(&self, id: WebhookId) -> Result<Option<Webhook>>;

    async fn list_active(&self) -> Result<Vec<Webhook>>;

    async fn list_all(&self) -> Result<Vec<Webhook>>;

    async fn patch(&self, id: WebhookId, patch: WebhookPatch) -> Result<Option<Webhook>>;

    async fn delete(&self, id: WebhookId) -> Result<bool>;

    /// Best-effort delivery log entry. Failure should be logged but not
    /// propagated to the bus subscriber.
    #[allow(clippy::too_many_arguments)]
    async fn record_delivery(
        &self,
        webhook_id: WebhookId,
        event_id: daruma_shared::EventId,
        event_kind: &str,
        status_code: Option<u16>,
        succeeded: bool,
        attempts: u32,
        error: Option<&str>,
    ) -> Result<()>;
}
