//! Pure conflict resolution helpers for multi-device event merge.

use std::cmp::Ordering;

use daruma_domain::Status;
use daruma_events::Event;
use daruma_shared::{EventId, Timestamp};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictRecord {
    pub event_id: EventId,
    pub occurred_at: Timestamp,
    pub op: ConflictOp,
    pub diff: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConflictOp {
    Create,
    Update,
    Delete,
    Status(Status),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConflictReason {
    DeleteWins,
    DoneMonotone,
    LastWriteWins,
}

impl ConflictReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConflictReason::DeleteWins => "delete_wins",
            ConflictReason::DoneMonotone => "done_monotone",
            ConflictReason::LastWriteWins => "last_write_wins",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictDecision {
    pub winner_event_id: EventId,
    pub loser_event_id: EventId,
    pub reason: ConflictReason,
    pub resolved_event: Event,
}

pub fn resolve_conflict(a: &ConflictRecord, b: &ConflictRecord) -> ConflictDecision {
    let (winner, loser, reason) = if a.op.is_delete() != b.op.is_delete() {
        if a.op.is_delete() {
            (a, b, ConflictReason::DeleteWins)
        } else {
            (b, a, ConflictReason::DeleteWins)
        }
    } else if a.op.is_done_status() != b.op.is_done_status() {
        if a.op.is_done_status() {
            (a, b, ConflictReason::DoneMonotone)
        } else {
            (b, a, ConflictReason::DoneMonotone)
        }
    } else if compare_lww(a, b) != Ordering::Less {
        (a, b, ConflictReason::LastWriteWins)
    } else {
        (b, a, ConflictReason::LastWriteWins)
    };

    ConflictDecision {
        winner_event_id: winner.event_id,
        loser_event_id: loser.event_id,
        reason: reason.clone(),
        resolved_event: Event::ConflictResolved {
            winner_event_id: winner.event_id,
            loser_event_id: loser.event_id,
            reason: reason.as_str().to_string(),
            loser_diff: loser.diff.clone(),
        },
    }
}

fn compare_lww(a: &ConflictRecord, b: &ConflictRecord) -> Ordering {
    match a.occurred_at.cmp(&b.occurred_at) {
        Ordering::Equal => a.event_id.cmp(&b.event_id),
        ord => ord,
    }
}

impl ConflictOp {
    fn is_delete(&self) -> bool {
        matches!(self, ConflictOp::Delete)
    }

    fn is_done_status(&self) -> bool {
        matches!(self, ConflictOp::Status(Status::Done))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Duration, Utc};
    use serde_json::json;

    fn ts(offset_secs: i64) -> Timestamp {
        DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + Duration::seconds(offset_secs)
    }

    fn rec(event_id: EventId, occurred_at: Timestamp, op: ConflictOp) -> ConflictRecord {
        ConflictRecord {
            event_id,
            occurred_at,
            op,
            diff: json!({"event": event_id.to_string()}),
        }
    }

    fn ids() -> (EventId, EventId) {
        (EventId::new(), EventId::new())
    }

    #[test]
    fn later_timestamp_wins_update_conflict() {
        let (a, b) = ids();
        let decision = resolve_conflict(
            &rec(a, ts(0), ConflictOp::Update),
            &rec(b, ts(1), ConflictOp::Update),
        );
        assert_eq!(decision.winner_event_id, b);
        assert_eq!(decision.reason, ConflictReason::LastWriteWins);
    }

    #[test]
    fn earlier_timestamp_loses_update_conflict() {
        let (a, b) = ids();
        let decision = resolve_conflict(
            &rec(a, ts(5), ConflictOp::Update),
            &rec(b, ts(1), ConflictOp::Update),
        );
        assert_eq!(decision.winner_event_id, a);
        assert_eq!(decision.loser_event_id, b);
    }

    #[test]
    fn event_id_breaks_equal_timestamp_tie() {
        let a = EventId::new();
        let b = EventId::new();
        let expected = std::cmp::max(a, b);
        let decision = resolve_conflict(
            &rec(a, ts(0), ConflictOp::Update),
            &rec(b, ts(0), ConflictOp::Update),
        );
        assert_eq!(decision.winner_event_id, expected);
    }

    #[test]
    fn delete_beats_later_update() {
        let (a, b) = ids();
        let decision = resolve_conflict(
            &rec(a, ts(0), ConflictOp::Delete),
            &rec(b, ts(10), ConflictOp::Update),
        );
        assert_eq!(decision.winner_event_id, a);
        assert_eq!(decision.reason, ConflictReason::DeleteWins);
    }

    #[test]
    fn delete_beats_earlier_update() {
        let (a, b) = ids();
        let decision = resolve_conflict(
            &rec(a, ts(0), ConflictOp::Update),
            &rec(b, ts(10), ConflictOp::Delete),
        );
        assert_eq!(decision.winner_event_id, b);
        assert_eq!(decision.reason, ConflictReason::DeleteWins);
    }

    #[test]
    fn delete_tie_uses_lww() {
        let (a, b) = ids();
        let decision = resolve_conflict(
            &rec(a, ts(0), ConflictOp::Delete),
            &rec(b, ts(1), ConflictOp::Delete),
        );
        assert_eq!(decision.winner_event_id, b);
        assert_eq!(decision.reason, ConflictReason::LastWriteWins);
    }

    #[test]
    fn done_beats_later_non_done_status() {
        let (a, b) = ids();
        let decision = resolve_conflict(
            &rec(a, ts(0), ConflictOp::Status(Status::Done)),
            &rec(b, ts(10), ConflictOp::Status(Status::InProgress)),
        );
        assert_eq!(decision.winner_event_id, a);
        assert_eq!(decision.reason, ConflictReason::DoneMonotone);
    }

    #[test]
    fn done_beats_update() {
        let (a, b) = ids();
        let decision = resolve_conflict(
            &rec(a, ts(0), ConflictOp::Update),
            &rec(b, ts(1), ConflictOp::Status(Status::Done)),
        );
        assert_eq!(decision.winner_event_id, b);
        assert_eq!(decision.reason, ConflictReason::DoneMonotone);
    }

    #[test]
    fn two_done_statuses_use_lww() {
        let (a, b) = ids();
        let decision = resolve_conflict(
            &rec(a, ts(0), ConflictOp::Status(Status::Done)),
            &rec(b, ts(1), ConflictOp::Status(Status::Done)),
        );
        assert_eq!(decision.winner_event_id, b);
        assert_eq!(decision.reason, ConflictReason::LastWriteWins);
    }

    #[test]
    fn create_vs_update_uses_lww() {
        let (a, b) = ids();
        let decision = resolve_conflict(
            &rec(a, ts(2), ConflictOp::Create),
            &rec(b, ts(1), ConflictOp::Update),
        );
        assert_eq!(decision.winner_event_id, a);
    }

    #[test]
    fn resolved_event_carries_loser_diff() {
        let (a, b) = ids();
        let decision = resolve_conflict(
            &ConflictRecord {
                diff: json!({"title": "old"}),
                ..rec(a, ts(0), ConflictOp::Update)
            },
            &rec(b, ts(1), ConflictOp::Update),
        );
        match decision.resolved_event {
            Event::ConflictResolved { loser_diff, .. } => {
                assert_eq!(loser_diff, json!({"title": "old"}));
            }
            _ => panic!("expected ConflictResolved"),
        }
    }

    #[test]
    fn resolved_event_carries_winner_and_loser_ids() {
        let (a, b) = ids();
        let decision = resolve_conflict(
            &rec(a, ts(0), ConflictOp::Update),
            &rec(b, ts(1), ConflictOp::Update),
        );
        match decision.resolved_event {
            Event::ConflictResolved {
                winner_event_id,
                loser_event_id,
                reason,
                ..
            } => {
                assert_eq!(winner_event_id, b);
                assert_eq!(loser_event_id, a);
                assert_eq!(reason, "last_write_wins");
            }
            _ => panic!("expected ConflictResolved"),
        }
    }
}
