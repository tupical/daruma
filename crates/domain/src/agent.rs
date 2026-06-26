use serde::{Deserialize, Serialize};
use daruma_shared::{time, AgentId, ProjectId, TaskId, Timestamp};

use crate::task::Priority;

/// Who initiated a command/event.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Actor {
    /// A human user using the desktop or web client.
    #[default]
    User,
    /// An AI agent. The `name` is a free-form identifier (e.g.
    /// "responses-gpt-4.1" or "local-parser").
    Agent { id: AgentId, name: String },
}

impl Actor {
    pub fn user() -> Self {
        Self::User
    }

    pub fn agent(name: impl Into<String>) -> Self {
        Self::Agent {
            id: AgentId::new(),
            name: name.into(),
        }
    }

    pub fn is_agent(&self) -> bool {
        matches!(self, Self::Agent { .. })
    }
}

/// What the agent suggests. Suggestions do **not** mutate state directly —
/// they become commands when accepted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentActionKind {
    SuggestTask {
        title: String,
        reason: String,
    },
    SuggestSplit {
        parent: TaskId,
        subtasks: Vec<String>,
    },
    SuggestPriority {
        task: TaskId,
        suggested: Priority,
        reason: String,
    },
    SummarizeProject {
        project_id: ProjectId,
        summary: String,
    },
    SuggestNextAction {
        text: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentAction {
    pub agent_name: String,
    pub kind: AgentActionKind,
    pub created_at: Timestamp,
}

impl AgentAction {
    pub fn new(agent_name: impl Into<String>, kind: AgentActionKind) -> Self {
        Self {
            agent_name: agent_name.into(),
            kind,
            created_at: time::now(),
        }
    }
}
