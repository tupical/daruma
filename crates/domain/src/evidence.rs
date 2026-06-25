//! Evidence registry (OSS task `019eb65a-3185`). Evidence is the *proof* that a
//! lifecycle [`Requirement`](crate::Requirement) was satisfied — a read
//! acknowledgement, an impact assessment, a completion note, an owner
//! assignment, etc. It is the carrier the rule engine queries to decide whether
//! a `required` rule blocks or passes (docs/LIFECYCLE_RULES_SPEC.md §1.3).
//!
//! Two invariants shape the model:
//!
//! 1. **Immutable.** Evidence is never edited in place — only recorded, or
//!    *superseded* by a newer record (`superseded_by`). The event log keeps the
//!    full history; the projection carries `superseded_by` so the gate can
//!    ignore retracted proof.
//! 2. **Distinct from the artifact registry (migration 0036).** Artifacts are
//!    *production outputs*; evidence is *process proof*. They never share rows.
//!
//! Evidence is scoped exactly like a [`RuleScope`](crate::RuleScope) so the gate
//! can walk the same tenant→project→plan→task chain it already assembles for
//! rule lookup: evidence recorded at any scope in the chain satisfies a rule
//! firing in that chain.

use serde::{Deserialize, Serialize};

use crate::RuleScope;
use daruma_shared::{
    AgentId, ArtifactId, EvidenceId, PlanId, ProjectId, RuleId, RunId, TaskId, Timestamp,
};

/// What a piece of evidence demonstrates. 1:1 with the lifecycle
/// [`Requirement`](crate::Requirement) discriminants (the wire strings match
/// `Requirement::type_str()`), so the gate maps a requirement to the kind of
/// evidence that satisfies it without translation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// A (versioned) document/artifact was read and acknowledged.
    DocumentReadAck,
    /// The impact of a change was assessed.
    ImpactAssessment,
    /// A decision was recorded.
    DecisionRecord,
    /// A who/when/why completion note was attached.
    CompletionNote,
    /// A named artifact was created.
    ArtifactCreated,
    /// An owner was assigned.
    OwnerAssigned,
    /// Acceptance criteria were defined.
    AcceptanceCriteriaDefined,
    /// A risk check was completed.
    RiskCheckCompleted,
}

impl EvidenceKind {
    /// Stable discriminant stored in the `kind` column. Identical to the
    /// matching `Requirement::type_str()` so the gate can compare directly.
    pub fn as_str(&self) -> &'static str {
        match self {
            EvidenceKind::DocumentReadAck => "document_read_ack",
            EvidenceKind::ImpactAssessment => "impact_assessment",
            EvidenceKind::DecisionRecord => "decision_record",
            EvidenceKind::CompletionNote => "completion_note",
            EvidenceKind::ArtifactCreated => "artifact_created",
            EvidenceKind::OwnerAssigned => "owner_assigned",
            EvidenceKind::AcceptanceCriteriaDefined => "acceptance_criteria_defined",
            EvidenceKind::RiskCheckCompleted => "risk_check_completed",
        }
    }

    /// Parse a stored discriminant. `None` for an unknown string (forward-
    /// compatible: a newer producer's kind is simply not matched, never panics).
    pub fn parse_str(s: &str) -> Option<Self> {
        Some(match s {
            "document_read_ack" => EvidenceKind::DocumentReadAck,
            "impact_assessment" => EvidenceKind::ImpactAssessment,
            "decision_record" => EvidenceKind::DecisionRecord,
            "completion_note" => EvidenceKind::CompletionNote,
            "artifact_created" => EvidenceKind::ArtifactCreated,
            "owner_assigned" => EvidenceKind::OwnerAssigned,
            "acceptance_criteria_defined" => EvidenceKind::AcceptanceCriteriaDefined,
            "risk_check_completed" => EvidenceKind::RiskCheckCompleted,
            _ => return None,
        })
    }
}

/// A recorded piece of evidence (immutable once persisted).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub id: EvidenceId,
    pub kind: EvidenceKind,
    /// Where the evidence applies (mirrors [`RuleScope`]). The gate walks the
    /// scope chain and treats evidence at any enclosing scope as satisfying.
    pub scope: RuleScope,
    /// Optional discriminator matching a requirement's `target` / `doc_ref`
    /// (e.g. the doc a `read_artifact` rule names, the `impact_check` target).
    /// `None` = evidence applies to the requirement regardless of target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// For `document_read_ack`: the document version that was read
    /// (`entity_versions`, migration 0020). `None` for other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_version: Option<String>,
    /// Who recorded the evidence.
    pub actor: ActorRef,
    /// Free-form why/details (the completion note text, the assessment, …).
    #[serde(default)]
    pub reason: String,
    /// Structured payload (required_fields content, etc.). Defaults to `null`.
    #[serde(default)]
    pub payload: serde_json::Value,
    // ── optional bindings (any subset) ──────────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<PlanId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<ArtifactId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<RuleId>,
    pub recorded_at: Timestamp,
    /// When set, a newer record superseded this one — the gate ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<EvidenceId>,
}

/// Lightweight, copy-able actor reference stored on evidence. Kept here rather
/// than reusing [`Actor`](crate::Actor) so the row stays a flat
/// `(kind, id, name)` triple matching the `entity_versions` convention.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorRef {
    /// `user` | `agent`.
    pub kind: String,
    /// `AgentId` for agents; `None` for the current-user actor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<AgentId>,
    /// Agent display name when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ActorRef {
    /// Project a domain [`Actor`](crate::Actor) into the stored triple.
    pub fn from_actor(actor: &crate::Actor) -> Self {
        match actor {
            crate::Actor::User => ActorRef {
                kind: "user".into(),
                id: None,
                name: None,
            },
            crate::Actor::Agent { id, name } => ActorRef {
                kind: "agent".into(),
                id: Some(*id),
                name: Some(name.clone()),
            },
        }
    }
}

/// Input for `RecordEvidence`. `id` is server-assigned when absent; the actor
/// and timestamp are filled by the handler so the record is deterministic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<EvidenceId>,
    pub kind: EvidenceKind,
    pub scope: RuleScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_version: Option<String>,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<PlanId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<ArtifactId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<RuleId>,
    /// When set, this record supersedes an earlier one (immutability: the old
    /// row is marked, not edited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<EvidenceId>,
}

impl NewEvidence {
    /// Materialise a stored [`Evidence`], assigning an id, actor and timestamp.
    pub fn into_evidence(self, actor: ActorRef, now: Timestamp) -> Evidence {
        Evidence {
            id: self.id.unwrap_or_default(),
            kind: self.kind,
            scope: self.scope,
            target: self.target,
            doc_version: self.doc_version,
            actor,
            reason: self.reason,
            payload: self.payload,
            project_id: self.project_id,
            plan_id: self.plan_id,
            task_id: self.task_id,
            run_id: self.run_id,
            artifact_id: self.artifact_id,
            rule_id: self.rule_id,
            recorded_at: now,
            superseded_by: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_round_trips_via_string() {
        for k in [
            EvidenceKind::DocumentReadAck,
            EvidenceKind::ImpactAssessment,
            EvidenceKind::DecisionRecord,
            EvidenceKind::CompletionNote,
            EvidenceKind::ArtifactCreated,
            EvidenceKind::OwnerAssigned,
            EvidenceKind::AcceptanceCriteriaDefined,
            EvidenceKind::RiskCheckCompleted,
        ] {
            assert_eq!(EvidenceKind::parse_str(k.as_str()), Some(k));
        }
        assert_eq!(EvidenceKind::parse_str("nope"), None);
    }

    #[test]
    fn kind_serde_is_snake_case() {
        let json = serde_json::to_string(&EvidenceKind::DocumentReadAck).unwrap();
        assert_eq!(json, "\"document_read_ack\"");
    }

    #[test]
    fn new_evidence_materialises_with_actor_and_time() {
        let now = daruma_shared::time::now();
        let ev = NewEvidence {
            id: None,
            kind: EvidenceKind::CompletionNote,
            scope: RuleScope::Tenant,
            target: None,
            doc_version: None,
            reason: "done".into(),
            payload: serde_json::Value::Null,
            project_id: None,
            plan_id: None,
            task_id: None,
            run_id: None,
            artifact_id: None,
            rule_id: None,
            supersedes: None,
        }
        .into_evidence(ActorRef::from_actor(&crate::Actor::User), now);
        assert_eq!(ev.kind, EvidenceKind::CompletionNote);
        assert_eq!(ev.actor.kind, "user");
        assert!(ev.superseded_by.is_none());
    }
}
