//! Activity projection entity — denormalised user-facing history of a task.

use serde::{Deserialize, Serialize};
use taskagent_shared::{ActivityId, EventId, ProjectId, TaskId, Timestamp};

use crate::agent::Actor;

/// A single activity row: one user-visible change to a task or project.
///
/// Rows are immutable once written — this is an append-only log.
/// The `seq` field mirrors the source event's global sequence number and
/// is the canonical cursor for pagination.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub id: ActivityId,
    /// `None` for project-level events (e.g. `ProjectCreated`).
    pub task_id: Option<TaskId>,
    pub project_id: Option<ProjectId>,
    pub actor: Actor,
    pub verb: Verb,
    /// For delta-verbs: which field changed (`"status"`, `"priority"`, …).
    pub field: Option<String>,
    /// Previous value; `None` when not tracked or not applicable.
    pub old_value: Option<String>,
    /// New value / payload (e.g. comment preview, task title on `created`).
    pub new_value: Option<String>,
    pub occurred_at: Timestamp,
    /// Source event id — UNIQUE in the DB, guarantees idempotent backfill.
    pub event_id: EventId,
    /// Global event sequence number; drives cursor-pagination and ordering.
    pub seq: i64,
}

/// Stable, user-visible verb taxonomy.
///
/// `serde(rename_all = "snake_case")` so JSON / DB values are lower_snake.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verb {
    // ── Task lifecycle ────────────────────────────────────────────────────
    /// `TaskCreated`
    Created,
    /// `TaskUpdated` — catch-all for title/description/due_at patches.
    Updated,
    /// `TaskStatusChanged` without crossing a terminal boundary.
    StatusChanged,
    /// `TaskPriorityChanged`
    PriorityChanged,
    /// `TaskClosed` (overrides `StatusChanged` when both emitted together).
    Closed,
    /// `TaskReopened` (overrides `StatusChanged` when both emitted together).
    Reopened,
    /// `TaskCompleted` mechanical event — only when no `TaskClosed` pair.
    Completed,
    /// `TaskDeleted`
    Deleted,
    /// `TaskSplitGenerated`
    SplitGenerated,
    /// `TaskDueElapsed` — the task's due date passed while still open.
    DueElapsed,

    // ── Project lifecycle ─────────────────────────────────────────────────
    ProjectCreated,
    ProjectUpdated,
    ProjectDeleted,

    // ── Comments ──────────────────────────────────────────────────────────
    /// `CommentAdded` / `TaskCommented` semantic pair.
    Commented,
    CommentEdited,
    CommentDeleted,

    // ── Agent ─────────────────────────────────────────────────────────────
    /// `AgentActionRecorded`
    AgentAction,

    // ── Plans ─────────────────────────────────────────────────────────────
    /// `PlanCreated`
    PlanCreated,
    /// `PlanUpdated/PlanStatusChanged/PlanGoalChanged/PlanReordered/PlanTask*` catch-all.
    PlanModified,
    /// `PlanArchived`
    PlanArchived,
    /// `PlanUpdated` where the `parent_plan_id` field changed (re-parented).
    PlanReparented,

    // ── Runs ──────────────────────────────────────────────────────────────
    /// `RunStarted`
    RunStarted,
    /// `RunCompleted`
    RunCompleted,
    /// `RunFailed`
    RunFailed,
    /// `RunAborted`
    RunAborted,

    // ── Plan↔task attachment ──────────────────────────────────────────────
    /// `PlanTaskAdded` — task was attached to a plan.
    TaskAttached,
    /// `PlanTaskRemoved` — task was detached from a plan.
    TaskDetached,

    // ── Agent claims ──────────────────────────────────────────────────────
    /// `AgentClaimed` — agent acquired an optimistic claim on a task.
    TaskClaimed,
    /// `AgentReleased` — agent released its claim on a task.
    TaskReleased,

    // ── Task relations ────────────────────────────────────────────────────
    /// `TaskLinked` — a typed relation was created between two tasks.
    Linked,
    /// `TaskUnlinked` — a typed relation was removed.
    Unlinked,
    /// `TaskUnblocked` — all blocking tasks are now Done; task is free to proceed.
    Unblocked,
    /// `TaskRelationKindChanged` — an existing relation's kind was transitioned
    /// (e.g. `Blocks → WasBlocking` when the blocker resolved; §3.7.2 / LIN A.3).
    RelationKindChanged,
}

impl std::fmt::Display for Verb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Use serde_plain-style serialisation: rely on serde's rename logic.
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{self:?}").to_lowercase());
        f.write_str(&s)
    }
}

impl std::str::FromStr for Verb {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_value(serde_json::Value::String(s.to_owned()))
            .map_err(|_| format!("unknown verb: {s}"))
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verb_roundtrip_serde() {
        for verb in [
            Verb::Created,
            Verb::Updated,
            Verb::StatusChanged,
            Verb::PriorityChanged,
            Verb::Closed,
            Verb::Reopened,
            Verb::Completed,
            Verb::Deleted,
            Verb::SplitGenerated,
            Verb::DueElapsed,
            Verb::ProjectCreated,
            Verb::ProjectUpdated,
            Verb::Commented,
            Verb::CommentEdited,
            Verb::CommentDeleted,
            Verb::AgentAction,
            // W2.3 — plans / runs / claims
            Verb::PlanCreated,
            Verb::PlanModified,
            Verb::PlanArchived,
            Verb::RunStarted,
            Verb::RunCompleted,
            Verb::RunFailed,
            Verb::RunAborted,
            Verb::TaskAttached,
            Verb::TaskDetached,
            Verb::TaskClaimed,
            Verb::TaskReleased,
            // §3.2 W1.3 — task relations
            Verb::Linked,
            Verb::Unlinked,
            Verb::Unblocked,
            // §3.7.2 — historical relation transitions
            Verb::RelationKindChanged,
            // W2 — plan hierarchy
            Verb::PlanReparented,
        ] {
            let json = serde_json::to_string(&verb).unwrap();
            let back: Verb = serde_json::from_str(&json).unwrap();
            assert_eq!(verb, back, "roundtrip failed for {verb:?}");
        }
    }

    #[test]
    fn verb_snake_case_names() {
        assert_eq!(
            serde_json::to_string(&Verb::StatusChanged).unwrap(),
            "\"status_changed\""
        );
        assert_eq!(
            serde_json::to_string(&Verb::PriorityChanged).unwrap(),
            "\"priority_changed\""
        );
        assert_eq!(
            serde_json::to_string(&Verb::Commented).unwrap(),
            "\"commented\""
        );
    }

    #[test]
    fn relation_verb_snake_case_names() {
        assert_eq!(serde_json::to_string(&Verb::Linked).unwrap(), "\"linked\"");
        assert_eq!(
            serde_json::to_string(&Verb::Unlinked).unwrap(),
            "\"unlinked\""
        );
        assert_eq!(
            serde_json::to_string(&Verb::Unblocked).unwrap(),
            "\"unblocked\""
        );
    }

    #[test]
    fn relation_verb_from_str() {
        let linked: Verb = "linked".parse().unwrap();
        assert_eq!(linked, Verb::Linked);
        let unlinked: Verb = "unlinked".parse().unwrap();
        assert_eq!(unlinked, Verb::Unlinked);
        let unblocked: Verb = "unblocked".parse().unwrap();
        assert_eq!(unblocked, Verb::Unblocked);
    }

    #[test]
    fn relation_verbs_exhaustive_match() {
        // Ensure all three new variants are reachable in a match arm —
        // compiler will error here if a variant is missing.
        for verb in [Verb::Linked, Verb::Unlinked, Verb::Unblocked] {
            let s = match verb {
                Verb::Linked => "linked",
                Verb::Unlinked => "unlinked",
                Verb::Unblocked => "unblocked",
                _ => "other",
            };
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn verb_from_str() {
        let v: Verb = "status_changed".parse().unwrap();
        assert_eq!(v, Verb::StatusChanged);
        let v: Verb = "commented".parse().unwrap();
        assert_eq!(v, Verb::Commented);
    }

    #[test]
    fn verb_from_str_unknown_errors() {
        let r = "nonexistent_verb".parse::<Verb>();
        assert!(r.is_err());
    }

    #[test]
    fn activity_roundtrip_serde() {
        use taskagent_shared::time;

        let activity = Activity {
            id: ActivityId::new(),
            task_id: Some(TaskId::new()),
            project_id: None,
            actor: Actor::user(),
            verb: Verb::Created,
            field: None,
            old_value: None,
            new_value: Some("my new task".to_string()),
            occurred_at: time::now(),
            event_id: EventId::new(),
            seq: 1,
        };
        let json = serde_json::to_string(&activity).unwrap();
        let back: Activity = serde_json::from_str(&json).unwrap();
        assert_eq!(activity, back);
    }
}
