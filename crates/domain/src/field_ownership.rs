//! Task field-ownership taxonomy (ADR-0007 Q1).
//!
//! ADR-0007 ("plan-only intake") splits a task's fields into two ownership
//! classes:
//!
//! - **Plan-owned** — the "что и зачем": materialised by the plan and
//!   immutable at the task level. They only change through a plan amend, never
//!   through a bare `UpdateTask`.
//! - **Execution-owned** — the "как идёт": free to change at the task level as
//!   work progresses.
//!
//! This module only *declares* the taxonomy; it centralises the decision so a
//! single source of truth exists. It intentionally carries **no behaviour** —
//! the `UpdateTask` gate that rejects patches to plan-owned fields is a
//! separate, later task in the ADR-0007 series (see the "Уточнённая разбивка"
//! item 2, `core`). Wiring the gate against this map is the next task's job.

use serde::{Deserialize, Serialize};

/// Which layer owns a task field per ADR-0007 Q1.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldOwner {
    /// Materialised by the plan; immutable at the task level (amend the plan).
    Plan,
    /// Free to change at the task level as execution progresses.
    Execution,
}

/// A task field whose ownership ADR-0007 Q1 classifies.
///
/// Membership (`plan_id` / `position` / `depends_on`) already lives only in
/// `plan_tasks`, so it is represented once as [`TaskField::Membership`] rather
/// than as three separate task columns.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskField {
    // ── plan-owned ────────────────────────────────────────────────────────
    Title,
    Description,
    ProjectId,
    /// Plan membership + its attributes (`plan_id`, `position`, `depends_on`),
    /// stored in `plan_tasks`.
    Membership,
    /// §3.8.10 provenance slot pointing at the `PlanCreated` event that
    /// materialised this task.
    SourceEventId,
    // ── execution-owned ───────────────────────────────────────────────────
    Status,
    TriageState,
    /// Plan-*seeded* but execution may re-weigh it, so it is execution-owned
    /// (ADR-0007 Q1).
    Priority,
    /// Scheduling knob. ADR-0007 does not name `due_at` explicitly; it is
    /// classified execution-owned under the "execution answers *how it goes*"
    /// principle (a schedule attribute, like [`TaskField::Priority`]).
    DueAt,
    StartedAt,
    CompletedAt,
    UpdatedBy,
    /// Comments attached to the task.
    Comments,
    /// Claims and runs recorded against the task.
    ClaimsRuns,
}

impl TaskField {
    /// The owning layer for this field per ADR-0007 Q1.
    pub fn owner(self) -> FieldOwner {
        match self {
            TaskField::Title
            | TaskField::Description
            | TaskField::ProjectId
            | TaskField::Membership
            | TaskField::SourceEventId => FieldOwner::Plan,
            TaskField::Status
            | TaskField::TriageState
            | TaskField::Priority
            | TaskField::DueAt
            | TaskField::StartedAt
            | TaskField::CompletedAt
            | TaskField::UpdatedBy
            | TaskField::Comments
            | TaskField::ClaimsRuns => FieldOwner::Execution,
        }
    }

    /// `true` when the field is materialised by the plan and immutable at the
    /// task level.
    pub fn is_plan_owned(self) -> bool {
        matches!(self.owner(), FieldOwner::Plan)
    }
}

/// The [`TaskPatch`](crate::TaskPatch) fields that ADR-0007 Q1 marks
/// plan-owned. The later `UpdateTask` gate rejects a patch that touches any of
/// these outside a plan amend.
///
/// Returned as the stable wire-format field names so a gate can report exactly
/// which fields it refused, matching the JSON a client sent.
pub fn plan_owned_patch_fields(patch: &crate::TaskPatch) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if patch.title.is_some() {
        fields.push("title");
    }
    if patch.description.is_some() {
        fields.push("description");
    }
    if patch.project_id.is_some() {
        fields.push("project_id");
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskPatch;

    #[test]
    fn plan_owned_fields_are_classified_plan() {
        for f in [
            TaskField::Title,
            TaskField::Description,
            TaskField::ProjectId,
            TaskField::Membership,
            TaskField::SourceEventId,
        ] {
            assert_eq!(f.owner(), FieldOwner::Plan, "{f:?} must be plan-owned");
            assert!(f.is_plan_owned());
        }
    }

    #[test]
    fn execution_owned_fields_are_classified_execution() {
        for f in [
            TaskField::Status,
            TaskField::TriageState,
            TaskField::Priority,
            TaskField::DueAt,
            TaskField::StartedAt,
            TaskField::CompletedAt,
            TaskField::UpdatedBy,
            TaskField::Comments,
            TaskField::ClaimsRuns,
        ] {
            assert_eq!(f.owner(), FieldOwner::Execution, "{f:?} must be execution-owned");
            assert!(!f.is_plan_owned());
        }
    }

    #[test]
    fn plan_owned_patch_fields_lists_only_plan_owned() {
        let patch = TaskPatch {
            title: Some("t".into()),
            description: Some("d".into()),
            status: Some(crate::Status::Todo),
            priority: Some(crate::Priority::P1),
            project_id: Some(None),
            ..Default::default()
        };
        let mut got = plan_owned_patch_fields(&patch);
        got.sort_unstable();
        // status/priority are execution-owned → must not appear.
        assert_eq!(got, vec!["description", "project_id", "title"]);
    }

    #[test]
    fn plan_owned_patch_fields_empty_for_execution_only_patch() {
        let patch = TaskPatch {
            status: Some(crate::Status::InProgress),
            priority: Some(crate::Priority::P0),
            ..Default::default()
        };
        assert!(plan_owned_patch_fields(&patch).is_empty());
    }
}
