//! Path-overlap logic for work leases — re-exported from `daruma-shared`.
//!
//! The implementation lives in `daruma_shared::path_lease` so the storage
//! layer (which sits below `core`) can run overlap checks inside its reserve
//! transaction. This module keeps the `daruma_core::path_lease` path stable
//! for route/handler callers.

pub use daruma_shared::path_lease::{normalize_lease_path, paths_overlap};
