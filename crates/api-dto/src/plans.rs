//! Plan-related response DTOs.

use serde::{Deserialize, Serialize};
use taskagent_domain::{Plan, PlanProgress};

/// Response from `GET /v1/plans/{id}`.
///
/// The server returns `{ "plan": Plan, "progress": PlanProgress }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanWithProgress {
    pub plan: Plan,
    pub progress: PlanProgress,
}
