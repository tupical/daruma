//! Command-side of the runtime.
//!
//! Layout:
//!   - [`Command`]        — canonical command schema (lead-authored contract).
//!   - [`CommandHandler`] — validates commands, emits events, runs projections.
//!   - [`CommandBus`]     — async entry point for transports (HTTP/WS/desktop).
//!
//! The schema in [`command`] is the **contract**. Do not deviate.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod bus;
pub mod command;
pub mod conflict;
pub mod embed;
pub mod handler;
pub mod path_lease;
pub mod plan_concurrency;
pub mod plan_readiness;
pub mod relation_enforcement;
pub mod repos;
pub mod search;

pub use bus::CommandBus;
pub use command::{Command, CommandEnvelope};
pub use handler::CommandHandler;
pub use plan_concurrency::{detect_parent_cycle, NextTask, NextTaskResolver, MAX_PARENT_DEPTH};
pub use plan_readiness::{can_start, plan_fanout, plan_graph};
