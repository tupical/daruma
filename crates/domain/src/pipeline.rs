//! Task position in a role pipeline and the canonical status derived from it
//! (ADR-0011 п.2, decision_id `mcpbox.workflow-policy-and-structured-role-artifacts`).
//!
//! The position lives in the task's `metadata_json` under `pipeline_position`;
//! the stage sequence itself is the workspace `workflow_policy`, owned by the
//! caller that advances the task. The caller resolves the next stage from the
//! policy and records it here together with the stage's role, so the canonical
//! status is a function of `(position, policy)` that the core can evaluate
//! without reading the policy again.
//!
//! A task with a position is *pipeline-managed*: its canonical status changes
//! only through [`canonical_status`] (command `advance_pipeline`); direct
//! status writes are rejected, except cancelling. A task without a position
//! behaves exactly as before.

use crate::task::Status;
use serde::{Deserialize, Serialize};

/// Role whose stage holds the task in review. `reconciler` is its alias, as in
/// workflow_policy role normalisation.
pub const REVIEW_ROLES: &[&str] = &["reviewer", "reconciler"];

/// Key of the position inside the task's `metadata_json` object.
pub const PIPELINE_POSITION_KEY: &str = "pipeline_position";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelinePosition {
    /// Index into the workflow_policy `stages[]` the task is currently at.
    pub stage_index: u32,
    /// Role of that stage, copied from the policy when the position was set.
    pub role: String,
    /// `true` once the last stage finished: the pipeline is complete.
    #[serde(default)]
    pub complete: bool,
}

impl PipelinePosition {
    pub fn at(stage_index: u32, role: impl Into<String>) -> Self {
        Self {
            stage_index,
            role: role.into(),
            complete: false,
        }
    }

    pub fn completed(stage_index: u32, role: impl Into<String>) -> Self {
        Self {
            complete: true,
            ..Self::at(stage_index, role)
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let role = self.role.trim();
        if role.is_empty() {
            return Err("pipeline_position.role must not be empty".into());
        }
        if role.len() > 64 {
            return Err("pipeline_position.role must not exceed 64 bytes".into());
        }
        Ok(())
    }
}

/// The single derivation of a pipeline-managed task's canonical status:
/// a completed pipeline is `done`, a review stage is `in_review`, any other
/// stage is `in_progress`. Uses only the existing statuses (ADR-0004).
pub fn canonical_status(position: &PipelinePosition) -> Status {
    if position.complete {
        Status::Done
    } else if REVIEW_ROLES.contains(&position.role.trim()) {
        Status::InReview
    } else {
        Status::InProgress
    }
}

/// Whether a direct status write to `to` is allowed for a task at `position`.
/// Unmanaged tasks accept every write; managed tasks accept only cancelling
/// (an owner abort outside the pipeline).
pub fn direct_status_write_allowed(position: Option<&PipelinePosition>, to: Status) -> bool {
    position.is_none() || to == Status::Cancelled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_stage_is_in_review_other_stages_in_progress() {
        assert_eq!(
            canonical_status(&PipelinePosition::at(0, "draft")),
            Status::InProgress
        );
        assert_eq!(
            canonical_status(&PipelinePosition::at(1, "builder")),
            Status::InProgress
        );
        assert_eq!(
            canonical_status(&PipelinePosition::at(2, "reviewer")),
            Status::InReview
        );
        assert_eq!(
            canonical_status(&PipelinePosition::at(2, "reconciler")),
            Status::InReview
        );
        assert_eq!(
            canonical_status(&PipelinePosition::completed(2, "reviewer")),
            Status::Done
        );
    }

    #[test]
    fn managed_task_accepts_only_cancel() {
        let pos = PipelinePosition::at(1, "builder");
        assert!(!direct_status_write_allowed(Some(&pos), Status::InReview));
        assert!(!direct_status_write_allowed(Some(&pos), Status::Done));
        assert!(direct_status_write_allowed(Some(&pos), Status::Cancelled));
        assert!(direct_status_write_allowed(None, Status::Done));
    }

    #[test]
    fn position_rejects_unknown_fields_and_empty_role() {
        let bad = serde_json::json!({"stage_index": 0, "role": "builder", "status": "done"});
        assert!(serde_json::from_value::<PipelinePosition>(bad).is_err());
        assert!(PipelinePosition::at(0, " ").validate().is_err());
    }
}
