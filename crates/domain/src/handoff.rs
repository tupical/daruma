//! Handoff contract — a first-class gate between two work units (P5,
//! ADR docs/adr/work-units-and-artifacts.md).
//!
//! A handoff makes the knowledge transfer between units explicit instead of
//! burying it in comments: the producing unit *requests* a handoff naming the
//! artifacts and checklist the consumer needs; the consuming side *accepts*
//! or *rejects* it. `work_unit_drain_next` treats a non-accepted inbound
//! handoff as a not-ready reason — the consuming unit is not dispatched
//! until its inbound handoffs are accepted.
//!
//! One contract is live per `(from_work_unit, to_work_unit)` pair:
//! re-requesting after a rejection reopens the same contract (same id, new
//! payload) rather than accumulating rows.

use serde::{Deserialize, Serialize};
use daruma_shared::{AgentId, HandoffId, Timestamp, WorkUnitId};

/// Lifecycle of a handoff contract.
///
/// `Ready` and `Expired` are reserved: `Ready` is the future
/// artifact-registry integration ("required artifacts reached
/// `required_state`") and `Expired` a future TTL sweep. Neither transition
/// is wired yet; the gate today only distinguishes accepted from
/// everything else.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffStatus {
    #[default]
    Open,
    Ready,
    Accepted,
    Rejected,
    Expired,
}

impl HandoffStatus {
    /// Stable string form matching the serde snake_case representation.
    pub fn as_str(self) -> &'static str {
        match self {
            HandoffStatus::Open => "open",
            HandoffStatus::Ready => "ready",
            HandoffStatus::Accepted => "accepted",
            HandoffStatus::Rejected => "rejected",
            HandoffStatus::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "ready" => Some(Self::Ready),
            "accepted" => Some(Self::Accepted),
            "rejected" => Some(Self::Rejected),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }

    /// Does an inbound handoff in this status keep the consuming unit out
    /// of the dispatch pool? Everything except `Accepted`: an open or
    /// rejected (awaiting re-request) contract means the consumer does not
    /// yet have what it needs.
    pub fn blocks_dispatch(self) -> bool {
        !matches!(self, Self::Accepted)
    }
}

/// A handoff contract between two work units.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HandoffContract {
    pub id: HandoffId,
    /// The producing unit handing work over.
    pub from_work_unit_id: WorkUnitId,
    /// The consuming unit gated on this handoff.
    pub to_work_unit_id: WorkUnitId,
    /// Artifact URIs the consumer needs (`artifact://…`, `file://…`).
    #[serde(default)]
    pub required_artifact_ids: Vec<String>,
    /// State the artifacts must reach before the handoff is satisfiable:
    /// `draft | reviewed | approved | implemented | verified`. Advisory
    /// until the artifact-registry integration lands (`Ready` status).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_state: Option<String>,
    /// Acceptance checklist shown to the accepting side.
    #[serde(default)]
    pub checklist: Vec<String>,
    /// Who requested the handoff (the producing side's holder).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_agent_id: Option<AgentId>,
    /// Who accepted it; `None` until accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_by_agent_id: Option<AgentId>,
    pub status: HandoffStatus,
    /// Free-form acceptance notes / rejection reason (latest response).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Changes the rejecting side requires before re-request.
    #[serde(default)]
    pub required_changes: Vec<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Creation payload for `Command::RequestHandoff`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NewHandoffContract {
    pub from_work_unit_id: WorkUnitId,
    pub to_work_unit_id: WorkUnitId,
    #[serde(default)]
    pub required_artifact_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_state: Option<String>,
    #[serde(default)]
    pub checklist: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_agent_id: Option<AgentId>,
}

impl NewHandoffContract {
    /// Materialise into an open [`HandoffContract`]. `id` comes from the
    /// caller so a re-request can reuse the existing contract's id.
    pub fn into_contract(self, id: HandoffId, now: Timestamp) -> HandoffContract {
        HandoffContract {
            id,
            from_work_unit_id: self.from_work_unit_id,
            to_work_unit_id: self.to_work_unit_id,
            required_artifact_ids: self.required_artifact_ids,
            required_state: self.required_state,
            checklist: self.checklist,
            owner_agent_id: self.owner_agent_id,
            accepted_by_agent_id: None,
            status: HandoffStatus::Open,
            notes: None,
            required_changes: vec![],
            created_at: now,
            updated_at: now,
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_shared::time;

    #[test]
    fn status_roundtrip_and_blocking() {
        for s in [
            HandoffStatus::Open,
            HandoffStatus::Ready,
            HandoffStatus::Accepted,
            HandoffStatus::Rejected,
            HandoffStatus::Expired,
        ] {
            assert_eq!(HandoffStatus::parse(s.as_str()), Some(s));
            let json = serde_json::to_string(&s).unwrap();
            let back: HandoffStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
        assert!(HandoffStatus::Open.blocks_dispatch());
        assert!(HandoffStatus::Rejected.blocks_dispatch());
        assert!(!HandoffStatus::Accepted.blocks_dispatch());
    }

    #[test]
    fn new_contract_materialises_open() {
        let new = NewHandoffContract {
            from_work_unit_id: WorkUnitId::new(),
            to_work_unit_id: WorkUnitId::new(),
            required_artifact_ids: vec!["artifact://api/dashboard@v1".into()],
            required_state: Some("approved".into()),
            checklist: vec!["contract published".into()],
            owner_agent_id: None,
        };
        let contract = new.clone().into_contract(HandoffId::new(), time::now());
        assert_eq!(contract.status, HandoffStatus::Open);
        assert_eq!(contract.from_work_unit_id, new.from_work_unit_id);
        assert!(contract.accepted_by_agent_id.is_none());

        let json = serde_json::to_string(&contract).unwrap();
        let back: HandoffContract = serde_json::from_str(&json).unwrap();
        assert_eq!(contract, back);
    }
}
