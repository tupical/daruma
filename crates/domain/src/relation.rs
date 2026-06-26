//! Typed relations between tasks: Blocks, RelatesTo, Duplicates.

use serde::{Deserialize, Serialize};
use daruma_shared::{RelationId, TaskId, Timestamp};

use crate::agent::Actor;

/// The kind of directed relation between two tasks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    /// `from` blocks `to` — `to` cannot move to Done while `from` is not Done.
    Blocks,
    /// Informational link; no enforcement.
    RelatesTo,
    /// `from` duplicates `to`.
    Duplicates,
    /// `from` *was* blocking `to` — historical record of a `Blocks` edge that
    /// resolved when the blocker reached `Status::Done`. No enforcement; kept
    /// for audit. Edges in this state are excluded from active-blocker lookups
    /// and from cycle-detection (§3.7.2 / LIN A.3).
    WasBlocking,
}

/// A directed relation between two tasks.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Relation {
    pub id: RelationId,
    /// Direction-bearing source endpoint.
    pub from: TaskId,
    /// Direction-bearing target endpoint.
    pub to: TaskId,
    pub kind: RelationKind,
    pub created_at: Timestamp,
    pub created_by: Actor,
}

/// Read-projection shape returned by `GET /v1/tasks/{id}/relations`.
///
/// All five groups are relative to the task identified by `task_id` in the
/// request path; the underlying `Relation` records are included in full so
/// callers can inspect both endpoints.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRelations {
    /// Relations where `self.from == task_id` and `kind == Blocks`.
    pub blocks: Vec<Relation>,
    /// Relations where `self.to == task_id` and `kind == Blocks`.
    pub blocked_by: Vec<Relation>,
    /// Union of both directions where `kind == RelatesTo`.
    pub relates_to: Vec<Relation>,
    /// Relations where `self.from == task_id` and `kind == Duplicates`.
    pub duplicates: Vec<Relation>,
    /// Relations where `self.to == task_id` and `kind == Duplicates`.
    pub duplicated_by: Vec<Relation>,
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_shared::time;

    fn make_relation(kind: RelationKind) -> Relation {
        Relation {
            id: RelationId::new(),
            from: TaskId::new(),
            to: TaskId::new(),
            kind,
            created_at: time::now(),
            created_by: Actor::user(),
        }
    }

    #[test]
    fn relation_kind_serde_roundtrip() {
        for kind in [
            RelationKind::Blocks,
            RelationKind::RelatesTo,
            RelationKind::Duplicates,
            RelationKind::WasBlocking,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: RelationKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn relation_kind_snake_case_values() {
        assert_eq!(
            serde_json::to_value(RelationKind::Blocks).unwrap(),
            serde_json::json!("blocks")
        );
        assert_eq!(
            serde_json::to_value(RelationKind::RelatesTo).unwrap(),
            serde_json::json!("relates_to")
        );
        assert_eq!(
            serde_json::to_value(RelationKind::Duplicates).unwrap(),
            serde_json::json!("duplicates")
        );
        assert_eq!(
            serde_json::to_value(RelationKind::WasBlocking).unwrap(),
            serde_json::json!("was_blocking")
        );
    }

    #[test]
    fn relation_serde_roundtrip() {
        let rel = make_relation(RelationKind::Blocks);
        let json_val = serde_json::to_value(&rel).unwrap();
        let back: Relation = serde_json::from_value(json_val).unwrap();
        assert_eq!(rel.id, back.id);
        assert_eq!(rel.from, back.from);
        assert_eq!(rel.to, back.to);
        assert_eq!(rel.kind, back.kind);
        assert_eq!(rel.created_by, back.created_by);
    }

    #[test]
    fn task_relations_serialises() {
        let task_id = TaskId::new();
        let mut r = make_relation(RelationKind::Blocks);
        r.from = task_id;

        let tr = TaskRelations {
            blocks: vec![r],
            blocked_by: vec![],
            relates_to: vec![],
            duplicates: vec![],
            duplicated_by: vec![],
        };

        let v = serde_json::to_value(&tr).unwrap();
        assert!(v["blocks"].is_array());
        assert_eq!(v["blocks"].as_array().unwrap().len(), 1);
        assert!(v["blocked_by"].is_array());
        assert!(v["relates_to"].is_array());
        assert!(v["duplicates"].is_array());
        assert!(v["duplicated_by"].is_array());
    }
}
