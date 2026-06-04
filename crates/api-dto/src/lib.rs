//! Wire-level DTO types shared between the server and WASM frontend.
//!
//! This crate has **no** runtime dependencies (no tokio, sqlx, axum) so it
//! compiles to `wasm32-unknown-unknown` without modification.
//!
//! Consumers:
//! * `apps/server` — imports `MutationResponse`, `WsServerMessage`, etc.
//!   (re-exports these from the crates that used to define them, or defines
//!   them here as the single source of truth).
//! * `taskagent-web` — imports the same types, eliminating local mirrors.
//! * `crates/core` — re-exports `Command` / `CommandEnvelope` from here.
//! * `crates/sync` — re-exports `WsServerMessage` / `WsClientMessage` from here.

pub mod command;
pub mod mutation;
pub mod plans;
pub mod ws;

// Flat re-exports for convenience.
pub use command::{Command, CommandEnvelope};
pub use mutation::{MutationResponse, MutationWarning};
pub use plans::PlanWithProgress;
pub use ws::{WsClientMessage, WsServerMessage};
