//! Artifact Registry domain entities (P4).
//!
//! An Artifact is a named, versioned resource produced or consumed by agents.
//! Ownership is decoupled from claim: the `owner_agent_id` records *outcome*
//! accountability and is set independently of which agent currently holds a
//! work-lease on the underlying resource.

use serde::{Deserialize, Serialize};
use daruma_shared::{AgentId, ArtifactId, ArtifactRelationId, ProjectId, TaskId, Timestamp};

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
            created_at: now,
            updated_at: now,
        }
    }
}
