//! Real-time sync layer — bridges the in-process event bus to WebSocket
//! connections and exposes a command entry-point for WS transports.
//!
//! # Layout
//!
//! - [`Hub`]             — wires [`EventBus`] + [`CommandBus`] together.
//! - [`WsServerMessage`] — server → client wire type.
//! - [`WsClientMessage`] — client → server wire type.

pub mod hub;
pub mod wire;

pub use hub::{Hub, WsSubscription, WS_SUBSCRIBER_CHANNEL};
pub use wire::{WsClientMessage, WsServerMessage};
