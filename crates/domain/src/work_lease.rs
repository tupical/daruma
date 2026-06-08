//! WorkLease entity — a TTL'd file/path reservation held by an agent while it
//! works a task, so parallel agents never edit the same files.

use serde::{Deserialize, Serialize};
use taskagent_shared::{AgentId, ProjectId, TaskId, Timestamp, WorkLeaseId};

/// A single reserved path glob held by an agent for a task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkLease {
    pub id: WorkLeaseId,
    pub agent_id: AgentId,
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub path_glob: String,
    pub acquired_at: Timestamp,
    pub expires_at: Timestamp,
}
