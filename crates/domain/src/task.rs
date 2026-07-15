use daruma_shared::{time, EventId, ProjectId, TaskId, Timestamp};
use serde::{Deserialize, Serialize};

use crate::agent::Actor;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Inbox,
    Todo,
    InProgress,
    /// Work is staged for review/QA but not yet accepted. Non-terminal —
    /// the next transition is typically back to `InProgress` (rework) or
    /// forward to `Done`.
    InReview,
    Done,
    /// Terminal: the work was abandoned (e.g. a `Duplicates` relation
    /// auto-cancels the duplicate side per §3.2/§3.7.2). Distinct from
    /// `Done` so completion metrics don't conflate "shipped" with
    /// "won't do".
    Cancelled,
}

impl Status {
    pub fn is_terminal(self) -> bool {
        matches!(self, Status::Done | Status::Cancelled)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Inbox => "inbox",
            Status::Todo => "todo",
            Status::InProgress => "in_progress",
            Status::InReview => "in_review",
            Status::Done => "done",
            Status::Cancelled => "cancelled",
        }
    }

    /// Parse a stable status discriminant (the `as_str` form). `None` for an
    /// unknown string.
    pub fn parse_str(s: &str) -> Option<Self> {
        Some(match s {
            "inbox" => Status::Inbox,
            "todo" => Status::Todo,
            "in_progress" => Status::InProgress,
            "in_review" => Status::InReview,
            "done" => Status::Done,
            "cancelled" => Status::Cancelled,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    /// Urgent.
    P0,
    /// High.
    P1,
    /// Medium (default).
    #[default]
    P2,
    /// Low.
    P3,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::P0 => "p0",
            Priority::P1 => "p1",
            Priority::P2 => "p2",
            Priority::P3 => "p3",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriageState {
    #[default]
    Pending,
    Accepted,
    Rejected,
}

impl TriageState {
    pub fn as_str(self) -> &'static str {
        match self {
            TriageState::Pending => "pending",
            TriageState::Accepted => "accepted",
            TriageState::Rejected => "rejected",
        }
    }
}

/// Canonical task entity — a projection of the event log.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub project_id: Option<ProjectId>,
    pub title: String,
    pub description: String,
    pub status: Status,
    pub priority: Priority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_state: Option<TriageState>,
    pub due_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// Set on the first non-terminal-to-`InProgress` transition. Stays set
    /// across subsequent reopens (records the original start moment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    /// Set on terminal transition (`TaskClosed`), cleared on `TaskReopened`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<Timestamp>,
    /// Actor who created this task (from `TaskCreated` envelope).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Actor>,
    /// Actor who last completed/closed this task (from `TaskCompleted`/`TaskClosed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_by: Option<Actor>,
    /// Actor from the last event that changed this task projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<Actor>,
    /// Source event id for the last event that changed this task projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_event_id: Option<EventId>,
    /// Global event sequence for the last event that changed this task projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_event_seq: Option<u64>,
    /// §3.8.10 provenance: id of the upstream event that produced this
    /// task (e.g. the `PlanCreated` event for a plan-derived task, or
    /// an AI-tool envelope). Opaque blob; the producer chooses what to
    /// store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_event_id: Option<EventId>,
    /// Opaque idempotency key from an external source (webhook / importer).
    /// When set, it is unique within the workspace: a repeat `CreateTask`
    /// carrying the same `external_key` upserts onto the existing task
    /// instead of spawning a duplicate. `None` for tasks with no external
    /// origin. Serialised as omitted (not `null`) when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_key: Option<String>,
}

impl Task {
    /// Build a [`Task`] from a [`NewTask`], filling defaults.
    pub fn from_new(input: NewTask) -> Self {
        let now = time::now();
        Self {
            id: input.id.unwrap_or_default(),
            project_id: input.project_id,
            title: input.title,
            description: input.description.unwrap_or_default(),
            status: input.status.unwrap_or_default(),
            priority: input.priority.unwrap_or_default(),
            triage_state: input.triage_state,
            due_at: input.due_at,
            created_at: now,
            updated_at: now,
            started_at: None,
            completed_at: None,
            created_by: None,
            completed_by: None,
            updated_by: None,
            updated_event_id: None,
            updated_event_seq: None,
            source_event_id: input.source_event_id,
            external_key: input.external_key,
        }
    }
}

/// Input for creating a task. Most fields optional.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NewTask {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_state: Option<TriageState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<Timestamp>,
    /// Optional external idempotency key (see [`Task::external_key`]). When
    /// present on `CreateTask`, a task already carrying the same key in this
    /// workspace is upserted (context appended as a comment) rather than
    /// duplicated. Fully optional — omitting it preserves legacy behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_key: Option<String>,
    /// §3.8.10 provenance (ADR-0007 Q5): the upstream event that produced this
    /// task. `MaterializePlan` sets this to the `PlanCreated` event id so a
    /// plan-derived task points back at its plan's creation. `None` for tasks
    /// created outside a plan-materialisation. The `TaskCreated` projection
    /// carries it onto [`Task::source_event_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_event_id: Option<EventId>,
}

impl NewTask {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            id: None,
            project_id: None,
            title: title.into(),
            description: None,
            status: None,
            priority: None,
            triage_state: None,
            due_at: None,
            external_key: None,
            source_event_id: None,
        }
    }
}

/// Sparse update for an existing task.
///
/// Field semantics:
/// - `None` outer = no change.
/// - `Some(Some(v))` = set to v.
/// - `Some(None)` (for nullable fields) = clear.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_state: Option<Option<TriageState>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<Option<Timestamp>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Option<ProjectId>>,
}

impl TaskPatch {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.description.is_none()
            && self.status.is_none()
            && self.priority.is_none()
            && self.triage_state.is_none()
            && self.due_at.is_none()
            && self.project_id.is_none()
    }

    pub fn apply(self, task: &mut Task) {
        if let Some(t) = self.title {
            task.title = t;
        }
        if let Some(d) = self.description {
            task.description = d;
        }
        if let Some(s) = self.status {
            task.status = s;
        }
        if let Some(p) = self.priority {
            task.priority = p;
        }
        if let Some(t) = self.triage_state {
            task.triage_state = t;
        }
        if let Some(d) = self.due_at {
            task.due_at = d;
        }
        if let Some(p) = self.project_id {
            task.project_id = p;
        }
        task.updated_at = time::now();
    }
}

/// Optional, human- or agent-supplied note attached to a `CompleteTask`
/// command. Backward compatible: omitted by legacy clients, in which case
/// `TaskCompleted` carries `completion_note: None` exactly as before.
///
/// The `actor` triple distinguishes a human-verified completion (`kind="user"`)
/// from an agent self-reported one (`kind="agent"`) at zero extra cost — it is
/// projected from the command's [`Actor`](crate::Actor) by the handler, so the
/// audit trail can tell the two apart (task risk note). Every field is
/// optional so a caller can supply just a `reason`, just a `result_summary`,
/// or the full set.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionNote {
    /// Who recorded the completion — user vs agent (filled by the handler).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<crate::ActorRef>,
    /// Why the task is considered done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// What was produced / the outcome summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    /// Free-form acceptance-criteria status (e.g. "3/3 met", "AC2 waived").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_criteria_status: Option<String>,
    /// References to artifacts produced (paths, URLs, doc refs, PR links).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_artifacts: Vec<String>,
}

impl CompletionNote {
    /// True when the note carries nothing beyond a possibly-filled actor — a
    /// caller passed an empty note. Used to avoid persisting noise.
    pub fn is_substantive(&self) -> bool {
        self.reason.is_some()
            || self.result_summary.is_some()
            || self.acceptance_criteria_status.is_some()
            || !self.related_artifacts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_terminal_covers_done_and_cancelled() {
        // The resolver and relation enforcement rely on this set being
        // exactly {Done, Cancelled}. If you add a new terminal Status,
        // update this test deliberately.
        assert!(!Status::Inbox.is_terminal());
        assert!(!Status::Todo.is_terminal());
        assert!(!Status::InProgress.is_terminal());
        assert!(!Status::InReview.is_terminal());
        assert!(Status::Done.is_terminal());
        assert!(Status::Cancelled.is_terminal());
    }

    #[test]
    fn status_serde_uses_snake_case() {
        // Wire-format contract: serde_json renders snake_case, which is
        // what the storage parse_status and the AI parse_status expect.
        let pairs = [
            (Status::Inbox, "\"inbox\""),
            (Status::Todo, "\"todo\""),
            (Status::InProgress, "\"in_progress\""),
            (Status::InReview, "\"in_review\""),
            (Status::Done, "\"done\""),
            (Status::Cancelled, "\"cancelled\""),
        ];
        for (status, expected) in pairs {
            assert_eq!(serde_json::to_string(&status).unwrap(), expected);
        }
    }
}
