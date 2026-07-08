//! Run entity — a single agent execution pass through a Plan.

use daruma_shared::{AgentId, PlanId, RunId, RunNoteId, Timestamp};
use serde::{Deserialize, Serialize};

use crate::agent::Actor;

/// Lifecycle status of a Run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Active,
    Completed,
    Failed,
    Aborted,
}

/// Terminal outcome of a Run step or Run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunOutcome {
    Done,
    /// Task removed from the plan while the run was executing.
    Superseded,
    /// Human marked Done before the agent finished.
    HumanCompleted,
    Skipped,
    Failed {
        reason: String,
    },
}

/// A single run of a Plan by an agent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
    pub plan_id: PlanId,
    pub agent_id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<RunId>,
    pub started_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
    pub status: RunStatus,
    /// Serialised terminal outcome; `None` while the run is still active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Wall-clock of the most recent run heartbeat (§3.7.4):
    /// `started_at` on `RunStarted`, refreshed on every step event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<Timestamp>,
    /// Set once when `RunUnresponsive` has been emitted for this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unresponsive_at: Option<Timestamp>,
    /// Set once when `RunStale` has been emitted for this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_at: Option<Timestamp>,
}

/// §3.8.2 — A free-form journal entry attached to a [`Run`].
///
/// Notes are append-only: there is no edit/delete command surface. Each note
/// captures who wrote it (`author`) and when (`created_at`), plus a body of
/// up to 4 KiB. See [`crate::run`] module docs and ROADMAP §3.8.2 for the
/// rationale (the agent needs somewhere to write narrative for a run that is
/// not the task description).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunNote {
    pub id: RunNoteId,
    pub run_id: RunId,
    pub body: String,
    pub author: Actor,
    pub created_at: Timestamp,
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_shared::{time, AgentId, PlanId, RunId};

    fn make_run() -> Run {
        Run {
            id: RunId::new(),
            plan_id: PlanId::new(),
            agent_id: AgentId::new(),
            parent_run_id: None,
            started_at: time::now(),
            ended_at: None,
            status: RunStatus::Active,
            outcome: None,
            last_activity_at: None,
            unresponsive_at: None,
            stale_at: None,
        }
    }

    #[test]
    fn run_roundtrip_serde() {
        let run = make_run();
        let json = serde_json::to_string(&run).unwrap();
        let back: Run = serde_json::from_str(&json).unwrap();
        assert_eq!(run, back);
    }

    #[test]
    fn run_with_parent_roundtrip_serde() {
        let mut run = make_run();
        run.parent_run_id = Some(RunId::new());
        run.ended_at = Some(time::now());
        run.outcome = Some("done".to_string());
        let json = serde_json::to_string(&run).unwrap();
        let back: Run = serde_json::from_str(&json).unwrap();
        assert_eq!(run, back);
    }

    #[test]
    fn run_status_roundtrip_serde() {
        for status in [
            RunStatus::Active,
            RunStatus::Completed,
            RunStatus::Failed,
            RunStatus::Aborted,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: RunStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back, "roundtrip failed for {status:?}");
        }
    }

    #[test]
    fn run_status_snake_case() {
        assert_eq!(
            serde_json::to_string(&RunStatus::Active).unwrap(),
            "\"active\""
        );
    }

    #[test]
    fn run_outcome_unit_variants_roundtrip_serde() {
        for outcome in [
            RunOutcome::Done,
            RunOutcome::Superseded,
            RunOutcome::HumanCompleted,
            RunOutcome::Skipped,
        ] {
            let json = serde_json::to_string(&outcome).unwrap();
            let back: RunOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(outcome, back, "roundtrip failed for {outcome:?}");
        }
    }

    #[test]
    fn run_outcome_failed_roundtrip_serde() {
        let outcome = RunOutcome::Failed {
            reason: "out of context".to_string(),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let back: RunOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome, back);
    }

    #[test]
    fn run_note_roundtrip_serde() {
        let note = RunNote {
            id: RunNoteId::new(),
            run_id: RunId::new(),
            body: "first observation".to_string(),
            author: Actor::user(),
            created_at: time::now(),
        };
        let json = serde_json::to_string(&note).unwrap();
        let back: RunNote = serde_json::from_str(&json).unwrap();
        assert_eq!(note, back);
    }

    #[test]
    fn run_outcome_tagged_kind_field() {
        let outcome = RunOutcome::Done;
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(
            json.contains("\"kind\""),
            "expected 'kind' tag field, got: {json}"
        );
        assert!(
            json.contains("\"done\""),
            "expected 'done' value, got: {json}"
        );
    }
}
