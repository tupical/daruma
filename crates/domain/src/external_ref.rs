//! ExternalRef — maps an external system's identifier to an internal entity ID.
//! Enables idempotent creation via `(tenant, kind, external_id)` uniqueness.

use serde::{Deserialize, Serialize};
use daruma_shared::Timestamp;

/// A cross-system identity mapping.
///
/// `tenant` identifies the source system (e.g. `"omc"`, `"github"`).
/// `kind` is the entity kind (e.g. `"plan"`, `"task"`, `"session"`).
/// `external_id` is the opaque identifier on the external side.
/// `internal_id` is the serialised form of the matching internal ID
/// (e.g. `PlanId.to_string()`, `TaskId.to_string()`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExternalRef {
    pub tenant: String,
    pub kind: String,
    pub external_id: String,
    pub internal_id: String,
    pub created_at: Timestamp,
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_shared::{time, PlanId};

    #[test]
    fn external_ref_roundtrip_serde() {
        let plan_id = PlanId::new();
        let ext = ExternalRef {
            tenant: "omc".to_string(),
            kind: "plan".to_string(),
            external_id: "plan-abc-123".to_string(),
            internal_id: plan_id.to_string(),
            created_at: time::now(),
        };
        let json = serde_json::to_string(&ext).unwrap();
        let back: ExternalRef = serde_json::from_str(&json).unwrap();
        assert_eq!(ext, back);
    }

    #[test]
    fn external_ref_github_roundtrip_serde() {
        let ext = ExternalRef {
            tenant: "github".to_string(),
            kind: "task".to_string(),
            external_id: "org/repo#42".to_string(),
            internal_id: "tsk_018f1234-5678-7abc-def0-123456789abc".to_string(),
            created_at: time::now(),
        };
        let json = serde_json::to_string(&ext).unwrap();
        let back: ExternalRef = serde_json::from_str(&json).unwrap();
        assert_eq!(ext, back);
    }

    #[test]
    fn external_ref_session_kind_roundtrip_serde() {
        use daruma_shared::AgentSessionId;
        let session_id = AgentSessionId::new();
        let ext = ExternalRef {
            tenant: "omc".to_string(),
            kind: "session".to_string(),
            external_id: "session-external-999".to_string(),
            internal_id: session_id.to_string(),
            created_at: time::now(),
        };
        let json = serde_json::to_string(&ext).unwrap();
        let back: ExternalRef = serde_json::from_str(&json).unwrap();
        assert_eq!(ext, back);
    }
}
