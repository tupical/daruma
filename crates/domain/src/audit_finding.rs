//! Audit findings (OSS task `019eb674-f289`; Audit primitives task B). A finding
//! is a problem a server-side audit check raised about an entity — an unread
//! document, a task stuck in its status, a missing owner, a duplicate-candidate
//! pair. Findings feed the Cloud Workspace Audit surface; the OSS core only
//! stores and serves them (no LLM in the loop).
//!
//! Two invariants shape the model, and both differ from the evidence registry
//! (which is *immutable* process proof):
//!
//! 1. **Upsert, not append.** A check is idempotent over its dedup key
//!    `(project_id, check_key, entity tuple)`: re-running it updates the existing
//!    finding's `last_seen_at` / `severity` / `detail` instead of inserting a
//!    duplicate. The dedup key lives in migration 0041's unique index.
//! 2. **Auto-resolve.** When a check no longer reproduces a finding it resolved
//!    earlier, the finding flips to `Resolved` rather than lingering as noise.

use serde::{Deserialize, Serialize};

use crate::ActorRef;
use daruma_shared::{ArtifactId, AuditFindingId, DocumentId, PlanId, ProjectId, TaskId, Timestamp};

/// How serious a finding is. Stable wire strings stored in the `severity` column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    /// A real problem that should block or be fixed.
    Error,
    /// Worth attention but not blocking.
    Warn,
    /// Informational only.
    Info,
}

impl FindingSeverity {
    /// Stable discriminant stored in the `severity` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            FindingSeverity::Error => "error",
            FindingSeverity::Warn => "warn",
            FindingSeverity::Info => "info",
        }
    }

    /// Parse a stored discriminant. `None` for an unknown string (forward-
    /// compatible — a newer producer's severity is simply unmatched, never panics).
    pub fn parse_str(s: &str) -> Option<Self> {
        Some(match s {
            "error" => FindingSeverity::Error,
            "warn" => FindingSeverity::Warn,
            "info" => FindingSeverity::Info,
            _ => return None,
        })
    }
}

/// Lifecycle of a finding. `Open` on first sight; an operator may acknowledge or
/// mute it; it resolves when the check stops reproducing it or an operator
/// resolves it explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    Open,
    Acknowledged,
    Muted,
    Resolved,
}

impl FindingStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            FindingStatus::Open => "open",
            FindingStatus::Acknowledged => "acknowledged",
            FindingStatus::Muted => "muted",
            FindingStatus::Resolved => "resolved",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        Some(match s {
            "open" => FindingStatus::Open,
            "acknowledged" => FindingStatus::Acknowledged,
            "muted" => FindingStatus::Muted,
            "resolved" => FindingStatus::Resolved,
            _ => return None,
        })
    }
}

/// Who produced a finding. `Script` = a deterministic server-side check;
/// `Ai` = a (Cloud-side) LLM pass. Stored in the `source` column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSource {
    #[default]
    Script,
    Ai,
}

impl FindingSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            FindingSource::Script => "script",
            FindingSource::Ai => "ai",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        Some(match s {
            "script" => FindingSource::Script,
            "ai" => FindingSource::Ai,
            _ => return None,
        })
    }
}

/// The entity a finding is about. Any subset of refs may be set; all `None`
/// means a project-level finding. Carried as its own struct so the dedup key in
/// the repo and the optional bindings stay together.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingEntity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<PlanId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<DocumentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<ArtifactId>,
}

/// A persisted audit finding (mutable: upserted by check key, auto-resolvable).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditFinding {
    pub id: AuditFindingId,
    pub project_id: ProjectId,
    #[serde(default, flatten)]
    pub entity: FindingEntity,
    /// Stable check identity for idempotency, e.g. `doc.unread`.
    pub check_key: String,
    /// Free taxonomy bucket, e.g. `hygiene`, `staleness`, `duplication`.
    pub category: String,
    pub severity: FindingSeverity,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub remediation: String,
    #[serde(default)]
    pub source: FindingSource,
    pub status: FindingStatus,
    pub first_seen_at: Timestamp,
    pub last_seen_at: Timestamp,
    /// Who resolved it (operator action or the auto-resolver). `None` while open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<ActorRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<Timestamp>,
}

/// Input for recording (upserting) a finding from a check run. `id`, timestamps,
/// and `status` are server-assigned: a fresh sighting opens at `first_seen_at =
/// last_seen_at = now`, a repeat sighting bumps `last_seen_at` on the existing row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewFinding {
    pub project_id: ProjectId,
    #[serde(default, flatten)]
    pub entity: FindingEntity,
    pub check_key: String,
    pub category: String,
    pub severity: FindingSeverity,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub remediation: String,
    #[serde(default)]
    pub source: FindingSource,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_round_trips() {
        for s in [
            FindingSeverity::Error,
            FindingSeverity::Warn,
            FindingSeverity::Info,
        ] {
            assert_eq!(FindingSeverity::parse_str(s.as_str()), Some(s));
        }
        assert_eq!(FindingSeverity::parse_str("nope"), None);
    }

    #[test]
    fn status_round_trips() {
        for s in [
            FindingStatus::Open,
            FindingStatus::Acknowledged,
            FindingStatus::Muted,
            FindingStatus::Resolved,
        ] {
            assert_eq!(FindingStatus::parse_str(s.as_str()), Some(s));
        }
        assert_eq!(FindingStatus::parse_str("nope"), None);
    }

    #[test]
    fn source_round_trips_and_defaults_to_script() {
        for s in [FindingSource::Script, FindingSource::Ai] {
            assert_eq!(FindingSource::parse_str(s.as_str()), Some(s));
        }
        assert_eq!(FindingSource::default(), FindingSource::Script);
    }

    #[test]
    fn severity_serde_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&FindingSeverity::Warn).unwrap(),
            "\"warn\""
        );
    }

    #[test]
    fn finding_entity_flattens_onto_finding() {
        let f = AuditFinding {
            id: AuditFindingId::new(),
            project_id: ProjectId::new(),
            entity: FindingEntity {
                task_id: Some(TaskId::new()),
                ..Default::default()
            },
            check_key: "task.stuck".into(),
            category: "staleness".into(),
            severity: FindingSeverity::Warn,
            title: "stuck".into(),
            detail: String::new(),
            remediation: String::new(),
            source: FindingSource::Script,
            status: FindingStatus::Open,
            first_seen_at: daruma_shared::time::now(),
            last_seen_at: daruma_shared::time::now(),
            resolved_by: None,
            resolved_at: None,
        };
        let json = serde_json::to_string(&f).unwrap();
        // entity is flattened: `task_id` sits at the top level, no `entity` key.
        assert!(json.contains("task_id"));
        assert!(!json.contains("\"entity\""));
        let back: AuditFinding = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
    }
}
