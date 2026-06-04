//! Plan entity — a goal with an ordered list of tasks an agent works through.

use serde::{Deserialize, Serialize};
use taskagent_shared::{time, PlanId, ProjectId, TaskId, Timestamp};

use crate::task::Status;

/// Deserialise `Option<Option<T>>` with proper three-way semantics:
/// - key absent  → `None`          (no change intended)
/// - key = null  → `Some(None)`    (unparent / clear)
/// - key = value → `Some(Some(v))` (set / re-parent)
fn deserialize_double_option<'de, T, D>(d: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

use crate::agent::Actor;

/// Status of a Plan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    #[default]
    Draft,
    Active,
    Completed,
    Abandoned,
}

/// Top-level plan entity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub id: PlanId,
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_plan_id: Option<PlanId>,
    pub title: String,
    pub description: String,
    pub goal: String,
    pub success_criteria: Vec<String>,
    pub status: PlanStatus,
    pub owner: Actor,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<Timestamp>,
    /// §3.8.10 provenance: free-text "brief" that produced this plan
    /// (typically the original user prompt). Opaque blob; the producer
    /// chooses what to store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_brief: Option<String>,
}

/// Associates a task with a plan at a given position.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanTask {
    pub plan_id: PlanId,
    pub task_id: TaskId,
    pub position: u32,
    /// Minimal DAG: IDs of tasks that must complete before this one.
    pub depends_on: Vec<TaskId>,
}

/// Derived progress snapshot — computed on read, never stored directly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanProgress {
    pub tasks_total: u32,
    pub tasks_done: u32,
    pub sub_plans_total: u32,
    pub sub_plans_done: u32,
    /// 0.0..=100.0
    pub completion_pct: f32,
}

/// Executor-oriented progress snapshot for a single plan's task list.
///
/// Counts only direct `plan_tasks` members (not nested sub-plans). Used by
/// `GET /v1/plans/{id}/progress` and `taskagent_plan_progress`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanProgressSummary {
    pub total: u32,
    pub done: u32,
    pub in_progress: u32,
    /// Tasks in `inbox` or `todo` (not yet started).
    pub todo: u32,
    /// First eligible task per [`NextTaskResolver`] when the plan is `Active`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_ready: Option<taskagent_shared::TaskId>,
}

/// Node in a plan execution graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanGraphNode {
    pub task_id: TaskId,
    pub position: u32,
    pub depends_on: Vec<TaskId>,
    pub title: String,
    pub status: Status,
}

/// Directed edge in a plan execution graph.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanGraphEdge {
    pub from: TaskId,
    pub to: TaskId,
    /// `depends_on` for plan-local dependencies, `blocks` for task relations.
    pub kind: String,
}

/// DAG-shaped read model for a plan's direct task list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanGraph {
    pub nodes: Vec<PlanGraphNode>,
    pub edges: Vec<PlanGraphEdge>,
}

/// One parallel execution wave for a plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanFanoutWave {
    pub wave: u32,
    pub tasks: Vec<TaskId>,
}

/// Blocker details returned by `taskagent_can_start`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CanStartBlocker {
    pub task_id: TaskId,
    pub title: String,
    pub status: Status,
}

/// Readiness result for starting or continuing work on a task.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CanStart {
    pub ready: bool,
    pub blockers: Vec<CanStartBlocker>,
    pub reason: String,
}

/// Sparse update for an existing Plan.
///
/// `None` outer = no change.  `Some(v)` = set to v.
///
/// `parent_plan_id` uses a three-way encoding:
/// - absent / `None`       → no change
/// - `Some(None)`          → unparent (set to NULL)
/// - `Some(Some(id))`      → re-parent to `id`
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_criteria: Option<Vec<String>>,
    /// Three-way parent field: absent = no change, `null` = unparent, `"<id>"` = re-parent.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_double_option"
    )]
    pub parent_plan_id: Option<Option<PlanId>>,
}

impl PlanPatch {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.description.is_none()
            && self.goal.is_none()
            && self.success_criteria.is_none()
            && self.parent_plan_id.is_none()
    }

    pub fn apply(self, plan: &mut Plan) {
        if let Some(t) = self.title {
            plan.title = t;
        }
        if let Some(d) = self.description {
            plan.description = d;
        }
        if let Some(g) = self.goal {
            plan.goal = g;
        }
        if let Some(sc) = self.success_criteria {
            plan.success_criteria = sc;
        }
        if let Some(p) = self.parent_plan_id {
            plan.parent_plan_id = p;
        }
        plan.updated_at = time::now();
    }
}

/// Input for creating a new Plan.
///
/// Analogous to [`taskagent_domain::NewTask`].  Optional fields default to
/// empty / absent when not supplied.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NewPlan {
    pub project_id: ProjectId,
    pub title: String,
    pub owner: Actor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_criteria: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_plan_id: Option<PlanId>,
}

impl NewPlan {
    /// Minimal constructor — all optional fields default to absent.
    pub fn new(title: impl Into<String>, project_id: ProjectId, owner: Actor) -> Self {
        Self {
            project_id,
            title: title.into(),
            owner,
            description: None,
            goal: None,
            success_criteria: None,
            parent_plan_id: None,
        }
    }

    /// Materialise into a full [`Plan`] given a pre-allocated id and wall-clock `now`.
    pub fn into_plan(self, id: PlanId, now: Timestamp) -> Plan {
        Plan {
            id,
            project_id: self.project_id,
            parent_plan_id: self.parent_plan_id,
            title: self.title,
            description: self.description.unwrap_or_default(),
            goal: self.goal.unwrap_or_default(),
            success_criteria: self.success_criteria.unwrap_or_default(),
            status: PlanStatus::default(),
            owner: self.owner,
            created_at: now,
            updated_at: now,
            archived_at: None,
            source_brief: None,
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use taskagent_shared::{PlanId, ProjectId, TaskId};

    fn make_plan() -> Plan {
        let now = time::now();
        Plan {
            id: PlanId::new(),
            project_id: ProjectId::new(),
            parent_plan_id: None,
            title: "Test plan".to_string(),
            description: "A description".to_string(),
            goal: "Achieve something".to_string(),
            success_criteria: vec!["criterion 1".to_string()],
            status: PlanStatus::Draft,
            owner: Actor::user(),
            created_at: now,
            updated_at: now,
            archived_at: None,
            source_brief: None,
        }
    }

    #[test]
    fn plan_roundtrip_serde() {
        let plan = make_plan();
        let json = serde_json::to_string(&plan).unwrap();
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
    }

    #[test]
    fn plan_with_parent_roundtrip_serde() {
        let mut plan = make_plan();
        plan.parent_plan_id = Some(PlanId::new());
        let json = serde_json::to_string(&plan).unwrap();
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
    }

    #[test]
    fn plan_status_roundtrip_serde() {
        for status in [
            PlanStatus::Draft,
            PlanStatus::Active,
            PlanStatus::Completed,
            PlanStatus::Abandoned,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: PlanStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back, "roundtrip failed for {status:?}");
        }
    }

    #[test]
    fn plan_status_snake_case() {
        assert_eq!(
            serde_json::to_string(&PlanStatus::Active).unwrap(),
            "\"active\""
        );
    }

    #[test]
    fn plan_task_roundtrip_serde() {
        let pt = PlanTask {
            plan_id: PlanId::new(),
            task_id: TaskId::new(),
            position: 0,
            depends_on: vec![TaskId::new()],
        };
        let json = serde_json::to_string(&pt).unwrap();
        let back: PlanTask = serde_json::from_str(&json).unwrap();
        assert_eq!(pt, back);
    }

    #[test]
    fn plan_progress_roundtrip_serde() {
        let progress = PlanProgress {
            tasks_total: 5,
            tasks_done: 2,
            sub_plans_total: 1,
            sub_plans_done: 0,
            completion_pct: 40.0,
        };
        let json = serde_json::to_string(&progress).unwrap();
        let back: PlanProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(progress, back);
    }

    #[test]
    fn plan_patch_roundtrip_serde() {
        let patch = PlanPatch {
            title: Some("New title".to_string()),
            description: None,
            goal: Some("New goal".to_string()),
            success_criteria: None,
            parent_plan_id: None,
        };
        let json = serde_json::to_string(&patch).unwrap();
        let back: PlanPatch = serde_json::from_str(&json).unwrap();
        assert_eq!(patch, back);
    }

    #[test]
    fn plan_patch_default_is_empty() {
        let patch = PlanPatch::default();
        assert!(patch.is_empty());
    }

    #[test]
    fn plan_patch_apply() {
        let mut plan = make_plan();
        let patch = PlanPatch {
            title: Some("Updated".to_string()),
            description: None,
            goal: None,
            success_criteria: None,
            parent_plan_id: None,
        };
        patch.apply(&mut plan);
        assert_eq!(plan.title, "Updated");
    }

    // ── parent_plan_id serde (absent / explicit-null / value) ─────────────────

    #[test]
    fn plan_patch_parent_absent() {
        // Key absent in JSON → None (no change intended)
        let json = r#"{"title":"x"}"#;
        let patch: PlanPatch = serde_json::from_str(json).unwrap();
        assert!(patch.parent_plan_id.is_none(), "absent key must yield None");
    }

    #[test]
    fn plan_patch_parent_unset_via_explicit_null() {
        // Explicit `null` value → Some(None) (unparent)
        let json = r#"{"parent_plan_id":null}"#;
        let patch: PlanPatch = serde_json::from_str(json).unwrap();
        assert_eq!(
            patch.parent_plan_id,
            Some(None),
            "explicit null must yield Some(None)"
        );
    }

    #[test]
    fn plan_patch_parent_reparent() {
        // String UUID value → Some(Some(id)) (re-parent)
        let id = PlanId::new();
        // Use serde_json::json! to serialise id with the same format serde uses,
        // avoiding any discrepancy with PlanId's Display impl.
        let json = serde_json::json!({ "parent_plan_id": id }).to_string();
        let patch: PlanPatch = serde_json::from_str(&json).unwrap();
        assert_eq!(
            patch.parent_plan_id,
            Some(Some(id)),
            "id string must yield Some(Some(id))"
        );
    }
}
