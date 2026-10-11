//! Artifact Registry domain entities (P4).
//!
//! An Artifact is a named, versioned resource produced or consumed by agents.
//! Ownership is decoupled from claim: the `owner_agent_id` records *outcome*
//! accountability and is set independently of which agent currently holds a
//! work-lease on the underlying resource.

use daruma_shared::{AgentId, ArtifactId, ArtifactRelationId, ProjectId, TaskId, Timestamp};
use serde::{Deserialize, Serialize};

/// Lifecycle status of an artifact.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    /// Registered but not yet produced.
    #[default]
    Pending,
    /// Being actively written by a holder.
    Active,
    /// A committed snapshot is available for consumption.
    Committed,
    /// Superseded or explicitly deprecated; kept for audit.
    Deprecated,
}

impl ArtifactStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactStatus::Pending => "pending",
            ArtifactStatus::Active => "active",
            ArtifactStatus::Committed => "committed",
            ArtifactStatus::Deprecated => "deprecated",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "active" => Some(Self::Active),
            "committed" => Some(Self::Committed),
            "deprecated" => Some(Self::Deprecated),
            _ => None,
        }
    }
}

/// Status of a role artifact (ADR-0011 п.3) — a second axis, independent of
/// the registry [`ArtifactStatus`] lifecycle. Only `proposed` moves:
/// `proposed → applied | rejected | superseded`; the other three are final.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleArtifactStatus {
    /// Offered by a role; the next role may take or ignore it.
    #[default]
    Proposed,
    /// Replaced by a later generation of the same (task, role, kind).
    Superseded,
    /// Taken by the consuming role.
    Applied,
    /// Explicitly declined by the consuming role.
    Rejected,
}

impl RoleArtifactStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RoleArtifactStatus::Proposed => "proposed",
            RoleArtifactStatus::Superseded => "superseded",
            RoleArtifactStatus::Applied => "applied",
            RoleArtifactStatus::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "proposed" => Some(Self::Proposed),
            "superseded" => Some(Self::Superseded),
            "applied" => Some(Self::Applied),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }

    /// Allowed transitions: only out of `proposed`, never back into it.
    pub fn can_transition_to(self, to: Self) -> bool {
        self == Self::Proposed && to != Self::Proposed
    }
}

/// Structured result of a workflow role (ADR-0011 п.3), stored on a registry
/// [`Artifact`] row rather than in a separate table. `generation` is the
/// 1-based sequence within `(task_id, role, kind)`; parallel instances of a
/// role each get their own generation, so several may be `proposed` at once.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RoleArtifact {
    pub role: String,
    pub kind: String,
    pub generation: i64,
    pub status: RoleArtifactStatus,
    #[serde(default)]
    pub payload: serde_json::Value,
}

impl RoleArtifact {
    /// Canonical registry URI of a role artifact.
    pub fn uri(task_id: TaskId, role: &str, kind: &str, generation: i64) -> String {
        format!("artifact://task/{task_id}/role/{role}/{kind}/{generation}")
    }
}

/// A named, versioned resource in the registry.
///
/// The `uri` is the canonical address used in lease targets and
/// `WorkUnit.artifact_refs`: `artifact://`, `file://`, `contract://`, `env://`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub id: ArtifactId,
    /// Canonical resource URI (e.g. `artifact://api/users`).
    pub uri: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub status: ArtifactStatus,
    /// Outcome owner — who is accountable for this artifact.
    /// Decoupled from the transient work-lease holder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_agent_id: Option<AgentId>,
    /// Task that produced (or is producing) this artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// Opaque version tag set on `WriteCommitted` (e.g. a git SHA, semver).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Fencing token of the lease that last committed a write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_write_token: Option<i64>,
    /// Set only for role artifacts (ADR-0011 п.3); plain registry artifacts
    /// and events recorded before the field existed carry `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_artifact: Option<RoleArtifact>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Typed relationship between two artifacts.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRelationKind {
    /// `from` depends on `to` as an input.
    DependsOn,
    /// `from` implements the interface/contract defined by `to`.
    Implements,
    /// `from` contains tests that verify `to`.
    Tests,
    /// `from` provides documentation for `to`.
    Documents,
    /// `from` supersedes / replaces `to`.
    Supersedes,
    /// `from` and `to` are known to conflict when co-present.
    ConflictsWith,
}

impl ArtifactRelationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactRelationKind::DependsOn => "depends_on",
            ArtifactRelationKind::Implements => "implements",
            ArtifactRelationKind::Tests => "tests",
            ArtifactRelationKind::Documents => "documents",
            ArtifactRelationKind::Supersedes => "supersedes",
            ArtifactRelationKind::ConflictsWith => "conflicts_with",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "depends_on" => Some(Self::DependsOn),
            "implements" => Some(Self::Implements),
            "tests" => Some(Self::Tests),
            "documents" => Some(Self::Documents),
            "supersedes" => Some(Self::Supersedes),
            "conflicts_with" => Some(Self::ConflictsWith),
            _ => None,
        }
    }

    /// Edge label used in the workspace graph.
    pub fn graph_edge_kind(self) -> &'static str {
        match self {
            ArtifactRelationKind::DependsOn => "ArtDependsOn",
            ArtifactRelationKind::Implements => "ArtImplements",
            ArtifactRelationKind::Tests => "ArtTests",
            ArtifactRelationKind::Documents => "ArtDocuments",
            ArtifactRelationKind::Supersedes => "ArtSupersedes",
            ArtifactRelationKind::ConflictsWith => "ArtConflictsWith",
        }
    }
}

/// A directional relation between two artifacts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRelation {
    pub id: ArtifactRelationId,
    pub from_id: ArtifactId,
    pub to_id: ArtifactId,
    pub kind: ArtifactRelationKind,
    pub created_at: Timestamp,
}

/// Creation payload for `ArtifactRegistered`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NewArtifact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<ArtifactId>,
    pub uri: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
}

impl NewArtifact {
    pub fn into_artifact(self, now: Timestamp) -> Artifact {
        Artifact {
            id: self.id.unwrap_or_default(),
            uri: self.uri,
            title: self.title,
            description: self.description.unwrap_or_default(),
            status: ArtifactStatus::Pending,
            owner_agent_id: None,
            task_id: self.task_id,
            project_id: self.project_id,
            version: None,
            last_write_token: None,
            role_artifact: None,
            created_at: now,
            updated_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_status_transitions_only_out_of_proposed() {
        use RoleArtifactStatus::*;
        for to in [Applied, Rejected, Superseded] {
            assert!(Proposed.can_transition_to(to));
        }
        assert!(!Proposed.can_transition_to(Proposed));
        for from in [Applied, Rejected, Superseded] {
            for to in [Proposed, Applied, Rejected, Superseded] {
                assert!(!from.can_transition_to(to), "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn role_status_round_trips() {
        use RoleArtifactStatus::*;
        for s in [Proposed, Superseded, Applied, Rejected] {
            assert_eq!(RoleArtifactStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(RoleArtifactStatus::parse("pending"), None);
    }

    #[test]
    fn artifact_without_role_field_still_deserializes() {
        let json = serde_json::json!({
            "id": ArtifactId::new(),
            "uri": "artifact://api/users",
            "title": "t",
            "status": "pending",
            "created_at": "2026-10-11T00:00:00Z",
            "updated_at": "2026-10-11T00:00:00Z",
        });
        let a: Artifact = serde_json::from_value(json).unwrap();
        assert!(a.role_artifact.is_none());
    }
}
