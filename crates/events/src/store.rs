use async_trait::async_trait;
use daruma_shared::{EventId, Result};

use crate::envelope::EventEnvelope;

/// The append-only event log.
///
/// Implementations MUST assign a strictly-monotonic `seq` on append and
/// return it on the produced envelope.
#[async_trait]
pub trait EventStore: Send + Sync + 'static {
    /// Append a single envelope. Returns it with `seq` populated.
    async fn append(&self, envelope: EventEnvelope) -> Result<EventEnvelope>;

    /// Append a batch atomically (all-or-nothing).
    async fn append_batch(&self, envelopes: Vec<EventEnvelope>) -> Result<Vec<EventEnvelope>>;

    /// Load all events with `seq > since_seq`, ordered by seq ascending,
    /// capped at `limit`.
    async fn load_since(&self, since_seq: u64, limit: usize) -> Result<Vec<EventEnvelope>>;

    /// Load a single event by its stable idempotency key.
    async fn load_by_id(&self, id: EventId) -> Result<Option<EventEnvelope>>;

    /// Highest assigned sequence number (0 if the log is empty).
    async fn latest_seq(&self) -> Result<u64>;
}
