//! Plan-related response DTOs.

use daruma_domain::{Plan, PlanProgress};
use serde::{Deserialize, Serialize};

/// Response from `GET /v1/plans/{id}`.
///
/// The server returns `{ "plan": Plan, "progress": PlanProgress }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanWithProgress {
    pub plan: Plan,
    pub progress: PlanProgress,
}
