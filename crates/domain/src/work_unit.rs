//! WorkUnit entity — the minimal dispatchable unit of work, subordinate to
//! a task (ADR docs/adr/work-units-and-artifacts.md, P3). Work units only
//! materialize when the lazy-activation rule fires; simple tasks keep the
//! plain `task claim` path untouched.

use serde::{Deserialize, Serialize};
use taskagent_shared::{time, AgentId, PlanId, TaskId, Timestamp, WorkUnitId};

use crate::task::Priority;

/// Status of a WorkUnit. `Ready` means dependency/handoff gates are
/// satisfied and the unit is dispatchable; `Review` awaits steward
/// approval (handoff layer, P5).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkUnitStatus {
    #[default]
    Todo,
    Ready,
    InProgress,
    Blocked,
    Review,
    Done,
    Cancelled,
}

impl WorkUnitStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkUnitStatus::Todo => "todo",
            WorkUnitStatus::Ready => "ready",
            WorkUnitStatus::InProgress => "in_progress",
            WorkUnitStatus::Blocked => "blocked",
            WorkUnitStatus::Review => "review",
            WorkUnitStatus::Done => "done",
            WorkUnitStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "todo" => Some(Self::Todo),
            "ready" => Some(Self::Ready),
            "in_progress" => Some(Self::InProgress),
            "blocked" => Some(Self::Blocked),
            "review" => Some(Self::Review),
            "done" => Some(Self::Done),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
    }

    /// Dispatchable by `work_unit_drain_next`.
    pub fn is_dispatchable(self) -> bool {
        matches!(self, Self::Todo | Self::Ready)
    }
}

/// The minimal dispatchable unit of work. Ownership fields follow the ADR
/// accountability split: `owner_agent_id` is the transient *holder*
/// (claim), not the outcome owner — outcome ownership lives on artifacts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkUnit {
    pub id: WorkUnitId,
    /// Parent task this unit decomposes.
    pub task_id: TaskId,
    /// Optional stage (= plan with `parent_plan_id`, per ADR: no Stage table).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_plan_id: Option<PlanId>,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub status: WorkUnitStatus,
    pub priority: Priority,
    /// Capability tags for advisory scheduling (P6): `frontend`, `db`, …
    #[serde(default)]
    pub capability_tags: Vec<String>,
    /// Transient holder (claim); cleared on release/expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_expires_at: Option<Timestamp>,
    /// Declared resource URIs (P1 leases are acquired on claim):
    /// `file://`, `artifact://`, `contract://`, `env://`.
    #[serde(default)]
    pub artifact_refs: Vec<String>,
    /// Acceptance criteria returned in the dispatch briefing.
    #[serde(default)]
    pub acceptance: Vec<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Creation payload for `Command::CreateWorkUnit`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NewWorkUnit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<WorkUnitId>,
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_plan_id: Option<PlanId>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<WorkUnitStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    #[serde(default)]
    pub capability_tags: Vec<String>,
    #[serde(default)]
    pub artifact_refs: Vec<String>,
    #[serde(default)]
    pub acceptance: Vec<String>,
}

impl NewWorkUnit {
    pub fn into_work_unit(self, now: Timestamp) -> WorkUnit {
        WorkUnit {
            id: self.id.unwrap_or_default(),
            task_id: self.task_id,
            stage_plan_id: self.stage_plan_id,
            title: self.title,
            description: self.description.unwrap_or_default(),
            status: self.status.unwrap_or_default(),
            priority: self.priority.unwrap_or_default(),
            capability_tags: self.capability_tags,
            owner_agent_id: None,
            claim_expires_at: None,
            artifact_refs: self.artifact_refs,
            acceptance: self.acceptance,
            created_at: now,
            updated_at: now,
        }
    }
}

impl WorkUnit {
    pub fn sample(task_id: TaskId) -> Self {
        let now = time::now();
        Self {
            id: WorkUnitId::new(),
            task_id,
            stage_plan_id: None,
            title: "sample".into(),
            description: String::new(),
            status: WorkUnitStatus::Todo,
            priority: Priority::P2,
            capability_tags: vec![],
            owner_agent_id: None,
            claim_expires_at: None,
            artifact_refs: vec![],
            acceptance: vec![],
            created_at: now,
            updated_at: now,
        }
    }
}
