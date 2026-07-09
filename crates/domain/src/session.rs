//! AgentSession entity — Linear B.1: tracks an agent's live execution context
//! including its current plan steps.

use daruma_shared::{AgentId, AgentSessionId, SessionArtifactId, Timestamp};
use serde::{Deserialize, Serialize};

/// Status of a single plan step within an agent session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStepStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Canceled,
}

/// A single step entry in an agent session's current plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentSessionPlanStep {
    pub content: String,
    pub status: SessionStepStatus,
}

/// An agent execution session. Stores live plan steps (Linear B.1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: AgentSessionId,
    pub agent_id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<AgentId>,
    pub started_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
    pub plan_steps: Vec<AgentSessionPlanStep>,
    pub metadata: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionArtifactKind {
    File,
    Url,
    Diff,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionArtifact {
    pub id: SessionArtifactId,
    pub session_id: AgentSessionId,
    pub kind: SessionArtifactKind,
    #[serde(rename = "ref")]
    pub reference: String,
    pub metadata: serde_json::Value,
    pub created_at: Timestamp,
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_shared::{time, AgentId, AgentSessionId};

    fn make_session() -> AgentSession {
        AgentSession {
            id: AgentSessionId::new(),
            agent_id: AgentId::new(),
            parent_agent_id: None,
            started_at: time::now(),
            ended_at: None,
            plan_steps: vec![
                AgentSessionPlanStep {
                    content: "Step one".to_string(),
                    status: SessionStepStatus::Pending,
                },
                AgentSessionPlanStep {
                    content: "Step two".to_string(),
                    status: SessionStepStatus::InProgress,
                },
            ],
            metadata: serde_json::json!({"model": "gpt-4"}),
        }
    }

    #[test]
    fn session_roundtrip_serde() {
        let session = make_session();
        let json = serde_json::to_string(&session).unwrap();
        let back: AgentSession = serde_json::from_str(&json).unwrap();
        assert_eq!(session, back);
    }

    #[test]
    fn session_with_parent_roundtrip_serde() {
        let mut session = make_session();
        session.parent_agent_id = Some(AgentId::new());
        session.ended_at = Some(time::now());
        let json = serde_json::to_string(&session).unwrap();
        let back: AgentSession = serde_json::from_str(&json).unwrap();
        assert_eq!(session, back);
    }

    #[test]
    fn session_step_status_roundtrip_serde() {
        for status in [
            SessionStepStatus::Pending,
            SessionStepStatus::InProgress,
            SessionStepStatus::Completed,
            SessionStepStatus::Canceled,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: SessionStepStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back, "roundtrip failed for {status:?}");
        }
    }

    #[test]
    fn session_step_status_snake_case() {
        assert_eq!(
            serde_json::to_string(&SessionStepStatus::InProgress).unwrap(),
            "\"in_progress\""
        );
    }

    #[test]
    fn agent_session_plan_step_roundtrip_serde() {
        let step = AgentSessionPlanStep {
            content: "do something".to_string(),
            status: SessionStepStatus::Completed,
        };
        let json = serde_json::to_string(&step).unwrap();
        let back: AgentSessionPlanStep = serde_json::from_str(&json).unwrap();
        assert_eq!(step, back);
    }

    #[test]
    fn session_empty_steps_roundtrip_serde() {
        let mut session = make_session();
        session.plan_steps = vec![];
        session.metadata = serde_json::Value::Null;
        let json = serde_json::to_string(&session).unwrap();
        let back: AgentSession = serde_json::from_str(&json).unwrap();
        assert_eq!(session, back);
    }

    #[test]
    fn session_artifact_roundtrip_serde() {
        let artifact = SessionArtifact {
            id: SessionArtifactId::new(),
            session_id: AgentSessionId::new(),
            kind: SessionArtifactKind::File,
            reference: "target/report.txt".into(),
            metadata: serde_json::json!({"bytes": 42}),
            created_at: time::now(),
        };
        let json = serde_json::to_string(&artifact).unwrap();
        let back: SessionArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(artifact, back);
    }
}
