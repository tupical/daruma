//! Outbound webhook delivery — subscribes to the in-process event bus,
//! filters envelopes against each `Webhook` row, and POSTs them to the
//! configured URL with an HMAC-SHA256 signature.
//!
//! Storage is plugged in via the [`WebhookStore`] trait; the SQLite-backed
//! implementation lives in `daruma-storage::WebhookRepo`.

pub mod dispatcher;
pub mod enrich;
pub mod model;
pub mod sign;
pub mod store;

pub use dispatcher::{spawn_dispatcher, DispatcherHandle};
pub use enrich::{build_context, EnrichmentSource, NoopEnrichment};
pub use model::{NewWebhook, Webhook, WebhookPatch};
pub use sign::sign_body_hex;
pub use store::WebhookStore;
