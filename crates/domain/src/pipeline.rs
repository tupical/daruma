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
//!
//! Fan-in barrier (ADR-0011 п.4): a stage with `fanout > 1` runs parallel
//! instances. Each instance reports only its own part (`close_pipeline_stage`,
//! recorded in `closed`); none of them writes the status. A `required` stage
//! cannot be left until all `fanout` instances closed — see [`fan_in_blocks`].

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
    /// Parallel instances of the stage, copied from the policy (default 1).
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub fanout: u32,
    /// Whether the stage gates progress, copied from the policy (default
    /// `true`). An optional stage may be left before its fan-in closes.
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub required: bool,
    /// Instances that reported their part of this stage closed, in order.
    /// Empty when the position is set; filled only by `close_pipeline_stage`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed: Vec<String>,
}

/// Upper bound of `fanout`, as in workflow_policy validation.
pub const MAX_STAGE_FANOUT: u32 = 16;

fn one() -> u32 {
    1
}

fn is_one(n: &u32) -> bool {
    *n == 1
}

fn yes() -> bool {
    true
}

fn is_yes(b: &bool) -> bool {
    *b
}

/// Result of one instance reporting its part of a stage closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FanIn {
    /// The instance had already reported: nothing changes.
    Repeat,
    /// Recorded; `closed` of `fanout` instances are done, the barrier holds.
    Pending { closed: u32, fanout: u32 },
    /// Recorded, and this report closed the barrier. Returned exactly once
    /// per stage entry: later reports are `Repeat` or rejected.
    Closed,
}

impl PipelinePosition {
    pub fn at(stage_index: u32, role: impl Into<String>) -> Self {
        Self {
            stage_index,
            role: role.into(),
            complete: false,
            fanout: 1,
            required: true,
            closed: Vec::new(),
        }
    }

    /// Stage with `fanout` parallel instances.
    pub fn fanout(mut self, fanout: u32) -> Self {
        self.fanout = fanout;
        self
    }

    /// Stage that does not gate progress (`required: false`).
    pub fn optional(mut self) -> Self {
        self.required = false;
        self
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
        if self.fanout == 0 || self.fanout > MAX_STAGE_FANOUT {
            return Err(format!(
                "pipeline_position.fanout must be 1..={MAX_STAGE_FANOUT}"
            ));
        }
        Ok(())
    }

    /// Whether every parallel instance of the stage reported closed.
    pub fn fan_in_closed(&self) -> bool {
        self.closed.len() as u32 >= self.fanout
    }

    /// Record that `instance` closed its part of this stage. Pure: the
    /// caller persists the updated position. Errors on a completed pipeline,
    /// an empty or oversized instance id, or a new instance once all
    /// `fanout` instances are already closed.
    pub fn close_instance(&mut self, instance: &str) -> Result<FanIn, String> {
        let instance = instance.trim();
        if instance.is_empty() || instance.len() > 128 {
            return Err("instance must be 1..=128 bytes".into());
        }
        if self.complete {
            return Err("pipeline_complete: the pipeline has no open stage".into());
        }
        if self.closed.iter().any(|c| c == instance) {
            return Ok(FanIn::Repeat);
        }
        if self.fan_in_closed() {
            return Err(format!(
                "fan_in_full: stage {} ({}) already has all {} instances closed",
                self.stage_index, self.role, self.fanout
            ));
        }
        self.closed.push(instance.to_string());
        Ok(if self.fan_in_closed() {
            FanIn::Closed
        } else {
            FanIn::Pending {
                closed: self.closed.len() as u32,
                fanout: self.fanout,
            }
        })
    }
}

/// The fan-in barrier, evaluated by the single status derivation before the
/// task leaves its current stage: `Some(reason)` while a required stage has
/// open instances. A completed pipeline, an optional stage or a single-instance
/// stage (its one instance is the caller that advances) never blocks.
pub fn fan_in_blocks(current: &PipelinePosition) -> Option<String> {
    if current.complete || !current.required || current.fanout <= 1 || current.fan_in_closed() {
        return None;
    }
    Some(format!(
        "fan_in_pending: stage {} ({}) closed {} of {} required instances; it \
         advances only after every instance reports through close_pipeline_stage",
        current.stage_index,
        current.role,
        current.closed.len(),
        current.fanout
    ))
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
    fn fan_in_holds_until_last_instance_and_closes_once() {
        let mut pos = PipelinePosition::at(1, "draft").fanout(3);
        assert!(fan_in_blocks(&pos).is_some());
        assert_eq!(
            pos.close_instance("draft-1"),
            Ok(FanIn::Pending {
                closed: 1,
                fanout: 3
            })
        );
        assert_eq!(pos.close_instance(" draft-1 "), Ok(FanIn::Repeat));
        assert_eq!(
            pos.close_instance("draft-2"),
            Ok(FanIn::Pending {
                closed: 2,
                fanout: 3
            })
        );
        // N-1 of N: the stage still blocks.
        let blocked = fan_in_blocks(&pos).unwrap();
        assert!(blocked.contains("closed 2 of 3"), "{blocked}");
        assert_eq!(pos.close_instance("draft-3"), Ok(FanIn::Closed));
        assert_eq!(fan_in_blocks(&pos), None);
        // Repeats after the close stay repeats; a fourth instance is refused.
        assert_eq!(pos.close_instance("draft-3"), Ok(FanIn::Repeat));
        assert!(pos
            .close_instance("draft-4")
            .unwrap_err()
            .starts_with("fan_in_full"));
        // Closing never changes the derived status.
        assert_eq!(canonical_status(&pos), Status::InProgress);
    }

    #[test]
    fn optional_or_single_stage_does_not_block() {
        assert_eq!(
            fan_in_blocks(&PipelinePosition::at(1, "draft").fanout(3).optional()),
            None
        );
        assert_eq!(fan_in_blocks(&PipelinePosition::at(1, "builder")), None);
        assert_eq!(
            fan_in_blocks(&PipelinePosition::completed(2, "reviewer")),
            None
        );
        let mut done = PipelinePosition::completed(2, "reviewer");
        assert!(done
            .close_instance("x")
            .unwrap_err()
            .starts_with("pipeline_complete"));
    }

    #[test]
    fn fan_in_fields_round_trip_and_stay_compact() {
        let plain = serde_json::to_value(PipelinePosition::at(0, "builder")).unwrap();
        assert_eq!(
            plain,
            serde_json::json!({"stage_index": 0, "role": "builder", "complete": false})
        );
        let old: PipelinePosition = serde_json::from_value(plain).unwrap();
        assert_eq!((old.fanout, old.required, old.closed.len()), (1, true, 0));
        let mut pos = PipelinePosition::at(1, "draft").fanout(2).optional();
        pos.close_instance("a").unwrap();
        let back: PipelinePosition =
            serde_json::from_value(serde_json::to_value(&pos).unwrap()).unwrap();
        assert_eq!(back, pos);
        assert!(PipelinePosition::at(0, "draft")
            .fanout(0)
            .validate()
            .is_err());
        assert!(PipelinePosition::at(0, "draft")
            .fanout(17)
            .validate()
            .is_err());
    }

    #[test]
    fn position_rejects_unknown_fields_and_empty_role() {
        let bad = serde_json::json!({"stage_index": 0, "role": "builder", "status": "done"});
        assert!(serde_json::from_value::<PipelinePosition>(bad).is_err());
        assert!(PipelinePosition::at(0, " ").validate().is_err());
    }
}
