//! Bearer-token authentication and capability-based authorization.
//!
//! Provides the [`ApiToken`] model, the [`Capability`] bit-flag set, the
//! per-request [`AuthContext`] extension, and a low-level token verifier
//! that the HTTP and WS layers wire into their own middleware.
//!
//! Token storage lives in `daruma-storage::TokenRepo` (the trait
//! [`TokenStore`] keeps this crate decoupled from the SQLite layer so the
//! verifier can be reused in process-internal contexts and tests).

pub mod capability;
pub mod context;
pub mod scope;
pub mod store;
pub mod token;
pub mod verify;

pub use capability::{Capabilities, Capability};
pub use context::{AuthContext, MissingCapability};
pub use scope::{ProjectFilter, TokenScope};
pub use store::TokenStore;
pub use token::{generate, verify_plaintext, ApiToken, NewTokenSpec, TokenKind, TokenSecret};
pub use verify::{verify_bearer, VerifyError};
