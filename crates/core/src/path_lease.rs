//! Path-overlap logic for work leases — re-exported from `taskagent-shared`.
//!
//! The implementation lives in `taskagent_shared::path_lease` so the storage
//! layer (which sits below `core`) can run overlap checks inside its reserve
//! transaction. This module keeps the `taskagent_core::path_lease` path stable
//! for route/handler callers.

pub use taskagent_shared::path_lease::{normalize_lease_path, paths_overlap};
