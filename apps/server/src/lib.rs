//! `taskagent-server` library crate.
//!
//! Exposes shared types (e.g. middleware, routes, state) so that integration
//! tests in `tests/` can import them without duplicating the source.

pub mod cors;
pub mod error;
pub mod mcp_downloads;
pub mod middleware;
pub mod routes;
pub mod state;
pub mod workspace_graph;
pub mod ws;
