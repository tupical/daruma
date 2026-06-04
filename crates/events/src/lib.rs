//! Event types, envelope, in-memory bus, and the [`EventStore`] trait.
//!
//! Storage implementations live in `taskagent-storage`.
//!
//! Feature flags:
//! - `runtime` (default: off): enables the tokio-backed [`EventBus`] and the
//!   async [`EventStore`] trait. Enable this feature in server-side crates.

#[cfg(feature = "runtime")]
pub mod bus;
pub mod envelope;
pub mod event;
#[cfg(feature = "runtime")]
pub mod store;

#[cfg(feature = "runtime")]
pub use bus::{EventBus, EventReceiver, EventSender};
pub use envelope::EventEnvelope;
pub use event::{Channel, Event};
#[cfg(feature = "runtime")]
pub use store::EventStore;
