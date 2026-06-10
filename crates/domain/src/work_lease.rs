//! WorkLease entity — a TTL'd resource reservation held by an agent while it
//! works a task. Originally file/path globs only; generalized to arbitrary
//! resource URIs with modes and fencing tokens (ADR
//! docs/adr/work-units-and-artifacts.md, P1).

use serde::{Deserialize, Serialize};
use taskagent_shared::{normalize_lease_path, AgentId, ProjectId, TaskId, Timestamp, WorkLeaseId};

/// How a lease holder intends to use the resource. Only `Exclusive`
/// conflicts: it blocks (and is blocked by) every non-`Intent` lease.
/// `SharedRead` and `Review` coexist with each other; `Intent` is purely
/// advisory and never hard-blocks anything.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseMode {
    #[default]
    Exclusive,
    SharedRead,
    Review,
    Intent,
}

impl LeaseMode {
    pub fn as_str(self) -> &'static str {
        match self {
            LeaseMode::Exclusive => "exclusive",
            LeaseMode::SharedRead => "shared_read",
            LeaseMode::Review => "review",
            LeaseMode::Intent => "intent",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "exclusive" => Some(LeaseMode::Exclusive),
            "shared_read" => Some(LeaseMode::SharedRead),
            "review" => Some(LeaseMode::Review),
            "intent" => Some(LeaseMode::Intent),
            _ => None,
        }
    }

    /// Conflict matrix: `intent` never conflicts; otherwise a pair
    /// conflicts iff at least one side is `exclusive` (a write). Two
    /// readers, two reviewers, or reader+reviewer coexist.
    pub fn conflicts_with(self, other: LeaseMode) -> bool {
        if self == LeaseMode::Intent || other == LeaseMode::Intent {
            return false;
        }
        self == LeaseMode::Exclusive || other == LeaseMode::Exclusive
    }
}

/// Canonicalize a lease target URI (ADR §Artifact URI scheme).
///
/// * `file://<path>` (or a bare path with no scheme) → `file://` + the same
///   normalization used by path leases (`normalize_lease_path`).
/// * Other schemes (`artifact://`, `contract://`, `env://`, …) → scheme
///   lowercased, remainder trimmed of whitespace and trailing slashes.
///
/// Canonical URIs are compared for exact equality except `file://`, which
/// uses glob overlap.
pub fn canonical_target_uri(raw: &str) -> String {
    let raw = raw.trim();
    match raw.split_once("://") {
        None => format!("file://{}", normalize_lease_path(raw)),
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("file") => {
            format!("file://{}", normalize_lease_path(rest))
        }
        Some((scheme, rest)) => {
            let scheme = scheme.to_ascii_lowercase();
            let rest = rest.trim().trim_end_matches('/');
            format!("{scheme}://{rest}")
        }
    }
}

/// True when two canonical target URIs contend for the same resource:
/// `file://` targets use glob/prefix overlap, every other scheme is an
/// exact match. Different schemes never overlap.
pub fn targets_overlap(a: &str, b: &str) -> bool {
    match (a.strip_prefix("file://"), b.strip_prefix("file://")) {
        (Some(pa), Some(pb)) => taskagent_shared::paths_overlap(pa, pb),
        (None, None) => a == b,
        _ => false,
    }
}

/// A single reserved resource held by an agent for a task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkLease {
    pub id: WorkLeaseId,
    pub agent_id: AgentId,
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// Legacy/glob form of the resource. For `file://` targets this is the
    /// normalized repo-relative glob; for other schemes it mirrors
    /// `target_uri` so pre-mode consumers still see a value.
    pub path_glob: String,
    /// Canonical resource URI (`file://`, `artifact://`, `contract://`,
    /// `env://`). `None` only for rows written before migration 0033.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_uri: Option<String>,
    /// Lease mode; pre-0033 rows and legacy JSON deserialize as `exclusive`.
    #[serde(default)]
    pub mode: LeaseMode,
    /// Monotonic per-resource fencing token issued at acquisition. A write
    /// carrying a token older than the resource's current sequence must be
    /// rejected. `None` only for pre-0033 rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fencing_token: Option<i64>,
    pub acquired_at: Timestamp,
    pub expires_at: Timestamp,
}

impl WorkLease {
    /// Canonical resource identity used for fencing sequence rows.
    pub fn resource_key(&self) -> String {
        self.target_uri
            .clone()
            .unwrap_or_else(|| canonical_target_uri(&self.path_glob))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_matrix() {
        use LeaseMode::*;
        assert!(Exclusive.conflicts_with(Exclusive));
        assert!(Exclusive.conflicts_with(SharedRead));
        assert!(Exclusive.conflicts_with(Review));
        assert!(!Exclusive.conflicts_with(Intent));
        assert!(!SharedRead.conflicts_with(SharedRead));
        assert!(!SharedRead.conflicts_with(Review));
        assert!(!Review.conflicts_with(Review));
        assert!(!Intent.conflicts_with(Intent));
    }

    #[test]
    fn canonicalization() {
        assert_eq!(canonical_target_uri("src/lib.rs"), "file://src/lib.rs");
        assert_eq!(
            canonical_target_uri("FILE://src/lib.rs"),
            "file://src/lib.rs"
        );
        assert_eq!(
            canonical_target_uri("Artifact://api/users/"),
            "artifact://api/users"
        );
        assert_eq!(
            canonical_target_uri("contract://api/dashboard@v1"),
            "contract://api/dashboard@v1"
        );
    }

    #[test]
    fn overlap_dispatches_by_scheme() {
        assert!(targets_overlap("file://src", "file://src/lib.rs"));
        assert!(!targets_overlap("file://src", "file://crates"));
        assert!(targets_overlap(
            "artifact://api/users",
            "artifact://api/users"
        ));
        assert!(!targets_overlap(
            "artifact://api/users",
            "artifact://api/orders"
        ));
        assert!(!targets_overlap("artifact://api/users", "file://api/users"));
        assert!(!targets_overlap(
            "env://staging-db",
            "contract://staging-db"
        ));
    }

    #[test]
    fn legacy_json_defaults_to_exclusive_mode() {
        let legacy = serde_json::json!({
            "id": uuid::Uuid::now_v7().to_string(),
            "agent_id": uuid::Uuid::now_v7().to_string(),
            "task_id": uuid::Uuid::now_v7().to_string(),
            "path_glob": "src/a",
            "acquired_at": "2026-01-01T00:00:00Z",
            "expires_at": "2026-01-01T01:00:00Z",
        });
        let lease: WorkLease = serde_json::from_value(legacy).unwrap();
        assert_eq!(lease.mode, LeaseMode::Exclusive);
        assert_eq!(lease.fencing_token, None);
        assert_eq!(lease.resource_key(), "file://src/a");
    }
}
