use serde::{Deserialize, Serialize};
use taskagent_domain::{
    Actor, AgentAction, AgentSession, AgentSessionPlanStep, Comment, CommentPatch, Document,
    NewTask, Plan, PlanPatch, PlanStatus, Priority, Project, RelationKind, Run, RunOutcome,
    SessionArtifact, Status, TaskPatch, WorkLease,
};
use taskagent_shared::{
    AgentId, AgentSessionId, CommentId, DocumentId, EventId, PlanId, ProjectId, RelationId, RunId,
    RunNoteId, TaskId, Timestamp,
};

/// The reason a run was made obsolete by a plan edit (used by
/// [`Event::RunObsolescedByPlanEdit`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObsolescenceKind {
    /// The plan was archived.
    Archived,
    /// The task currently being executed was removed from the plan.
    TaskRemoved,
    /// The plan goal changed while the run was active.
    GoalChanged,
}

/// All mutations to the system are represented as events. Events are
/// append-only; projections are derived in [`taskagent-storage`].
///
/// `#[serde(tag = "type")]` produces a tagged-union JSON layout that is
/// safe to consume from the web / WS clients without ambiguity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TaskCreated {
        task: NewTask,
    },
    TaskUpdated {
        task_id: TaskId,
        patch: TaskPatch,
    },
    TaskStatusChanged {
        task_id: TaskId,
        from: Status,
        to: Status,
    },
    TaskPriorityChanged {
        task_id: TaskId,
        from: Priority,
        to: Priority,
    },
    TaskCompleted {
        task_id: TaskId,
        completed_at: Timestamp,
    },
    TaskDeleted {
        task_id: TaskId,
    },
    TaskSplitGenerated {
        parent: TaskId,
        subtasks: Vec<NewTask>,
    },
    ConflictResolved {
        winner_event_id: EventId,
        loser_event_id: EventId,
        reason: String,
        loser_diff: serde_json::Value,
    },
    ProjectCreated {
        project: Project,
    },
    ProjectUpdated {
        project_id: ProjectId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<Option<String>>,
    },
    /// A project was deleted.  Only permitted when the project contains no
    /// tasks and no plans; emitted after both invariants have been verified
    /// by the command handler.
    ProjectDeleted {
        project_id: ProjectId,
    },
    AgentActionRecorded {
        action: AgentAction,
    },
    CommentAdded {
        comment: Comment,
    },
    CommentEdited {
        comment_id: CommentId,
        task_id: TaskId,
        patch: CommentPatch,
        edited_at: Timestamp,
    },
    CommentDeleted {
        comment_id: CommentId,
        task_id: TaskId,
        deleted_at: Timestamp,
    },

    // ── Semantic task events (Wave 2 / W2.1) ───────────────────────────────────
    //
    // Emitted *alongside* the mechanical `TaskStatusChanged` event whenever a
    // status transition crosses the terminal boundary. They give subscribers a
    // simple way to filter for "task done" / "task came back" without inspecting
    // status pairs.
    /// Task transitioned from a terminal state back to a non-terminal one.
    TaskReopened {
        task_id: TaskId,
        by: Actor,
        at: Timestamp,
    },

    /// Task transitioned from a non-terminal state to a terminal one
    /// (currently `Status::Done`).
    TaskClosed {
        task_id: TaskId,
        by: Actor,
        at: Timestamp,
    },

    /// A comment was added — emitted in addition to `CommentAdded` and
    /// carrying a short preview, so realtime channels for "task activity" can
    /// surface a one-liner without loading the full comment.
    TaskCommented {
        task_id: TaskId,
        comment_id: CommentId,
        author: Actor,
        /// First 80 characters of the comment body, in display order.
        preview: String,
    },

    // ── Plans — mechanical (Wave 1 / W1.3) ────────────────────────────────────
    /// A new plan was created.
    PlanCreated {
        plan: Plan,
    },

    /// Plan metadata (title / description / goal / success_criteria) was updated.
    PlanUpdated {
        plan_id: PlanId,
        patch: PlanPatch,
    },

    /// Plan lifecycle status changed (e.g. Draft → Active).
    PlanStatusChanged {
        plan_id: PlanId,
        from: PlanStatus,
        to: PlanStatus,
    },

    /// The plan goal field was replaced — emitted alongside `PlanUpdated` for
    /// subscribers that need the old value to detect semantic drift.
    PlanGoalChanged {
        plan_id: PlanId,
        from: String,
        to: String,
    },

    /// A task was appended to (or inserted into) the plan.
    PlanTaskAdded {
        plan_id: PlanId,
        task_id: TaskId,
        position: u32,
        /// IDs of sibling plan-tasks that must reach `Done` before this task
        /// becomes eligible for the resolver. `#[serde(default)]` so events
        /// persisted before this field existed deserialise to an empty list.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        depends_on: Vec<TaskId>,
    },

    /// A task was removed from the plan.
    PlanTaskRemoved {
        plan_id: PlanId,
        task_id: TaskId,
    },

    /// The task order within the plan was changed.
    PlanReordered {
        plan_id: PlanId,
        order: Vec<TaskId>,
    },

    /// The plan was archived (no further runs allowed).
    PlanArchived {
        plan_id: PlanId,
        at: Timestamp,
    },

    // ── Runs — mechanical (Wave 1 / W1.3) ─────────────────────────────────────
    /// An agent started executing a plan.
    RunStarted {
        run: Run,
    },

    /// The agent moved to the next task step within the run.
    RunStepStarted {
        run_id: RunId,
        task_id: TaskId,
        at: Timestamp,
    },

    /// The agent finished a task step (successfully or with a known outcome).
    RunStepFinished {
        run_id: RunId,
        task_id: TaskId,
        outcome: RunOutcome,
        at: Timestamp,
    },

    /// The run finished successfully — all tasks done.
    RunCompleted {
        run_id: RunId,
        at: Timestamp,
    },

    /// The run terminated with an error.
    RunFailed {
        run_id: RunId,
        reason: String,
        at: Timestamp,
    },

    /// The run was aborted (e.g. by plan archive or explicit stop).
    RunAborted {
        run_id: RunId,
        reason: String,
        at: Timestamp,
    },

    /// §3.7.4 — the run started but did not produce a first `RunStepStarted`
    /// within the configured ack window. Signal-only; run stays `Active`.
    RunUnresponsive {
        run_id: RunId,
        at: Timestamp,
    },

    /// §3.7.4 — an active run has produced no step activity for at least the
    /// configured idle window. Signal-only; run stays `Active`.
    RunStale {
        run_id: RunId,
        at: Timestamp,
    },

    /// §3.8.2 — a free-form journal entry was appended to an active run.
    /// `by_actor` is the envelope actor at the time of `AppendRunNote`;
    /// the projection writes the note into the `run_notes` table.
    RunNoteAppended {
        run_id: RunId,
        note_id: RunNoteId,
        body: String,
        by_actor: Actor,
        at: Timestamp,
    },

    // ── Agent sessions (Wave 1 / W1.3) ────────────────────────────────────────
    /// An agent session started.
    AgentSessionStarted {
        session: AgentSession,
    },

    /// An agent session ended.
    AgentSessionEnded {
        session_id: AgentSessionId,
        at: Timestamp,
    },

    /// The agent replaced its session plan-steps (Linear B.1).
    AgentSessionPlanUpdated {
        session_id: AgentSessionId,
        steps: Vec<AgentSessionPlanStep>,
    },

    /// A file/url/diff artifact was attached to an agent session.
    SessionArtifactAttached {
        artifact: SessionArtifact,
    },

    /// An agent acquired an optimistic claim on a task.
    AgentClaimed {
        agent_id: AgentId,
        task_id: TaskId,
        expires_at: Timestamp,
    },

    /// An agent released its claim on a task (or the claim expired).
    AgentReleased {
        agent_id: AgentId,
        task_id: TaskId,
    },

    /// An agent reserved one or more file/path leases for a task.
    FilesReserved {
        leases: Vec<WorkLease>,
    },

    /// All of an agent's file/path leases for a task were released
    /// (task completed, explicit release, or TTL expiry).
    FilesReleased {
        agent_id: AgentId,
        task_id: TaskId,
    },

    // ── Semantic plan/run events (Wave 1 / W1.3) ──────────────────────────────
    /// A human edited the plan while an agent run was active.
    PlanModifiedByHuman {
        plan_id: PlanId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        during_run_id: Option<RunId>,
    },

    /// Two or more actors are concurrently modifying the same task.
    TaskContested {
        task_id: TaskId,
        actors: Vec<Actor>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        field: Option<String>,
    },

    /// A run has become obsolete because the plan was edited beneath it.
    RunObsolescedByPlanEdit {
        run_id: RunId,
        plan_id: PlanId,
        kind: ObsolescenceKind,
    },

    // ── Run signals — Linear B.5 (Wave 1 / W1.3) ──────────────────────────────
    /// Someone (human or system) requested the run to stop.
    RunStopRequested {
        run_id: RunId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        by: Actor,
    },

    /// The run needs a human decision before it can proceed.
    RunElicitationRequested {
        run_id: RunId,
        prompt: String,
        choices: Vec<String>,
    },

    /// The run requires external auth before proceeding.
    RunAuthRequired {
        run_id: RunId,
        scope: String,
    },

    /// A human responded to an elicitation or intervention request.
    RunInterventionAccepted {
        run_id: RunId,
        choice: String,
        by: Actor,
    },

    // ── Task relations (§3.2 Wave 1.3) ────────────────────────────────────────
    /// A typed relation was created between two tasks.
    TaskLinked {
        relation_id: RelationId,
        /// The source (direction-bearing) endpoint.
        from: TaskId,
        /// The target endpoint.
        to: TaskId,
        kind: RelationKind,
        actor: Actor,
        occurred_at: Timestamp,
    },

    /// A typed relation was removed.
    TaskUnlinked {
        relation_id: RelationId,
        from: TaskId,
        to: TaskId,
        kind: RelationKind,
        occurred_at: Timestamp,
    },

    /// All blocking tasks for `task_id` are now Done; this task is free to
    /// proceed. Emitted in the same `append_batch` as the blocker's
    /// `TaskStatusChanged(to: Done)`.
    TaskUnblocked {
        task_id: TaskId,
        /// The task that just became Done, completing the last blocking
        /// dependency.
        unblocked_by: TaskId,
        occurred_at: Timestamp,
    },

    /// An active task's `due_at` has passed without the task closing.
    /// Emitted once per (task, due_at) value by the server's due-date
    /// watchdog tick; webhook subscribers receive it as `task.due`.
    TaskDueElapsed {
        task_id: TaskId,
        /// The deadline that elapsed (the task's `due_at` at notification time).
        due_at: Timestamp,
        at: Timestamp,
    },

    /// An existing relation's `kind` was transitioned (§3.7.2 / LIN A.3).
    ///
    /// Today this is emitted alongside `TaskUnblocked` when a blocker reaches
    /// `Status::Done`: each active `Blocks` edge from that blocker is flipped
    /// to `WasBlocking` so the historical dependency is retained for audit
    /// instead of silently lingering as an "active" Blocks row.
    TaskRelationKindChanged {
        relation_id: RelationId,
        from: TaskId,
        to: TaskId,
        from_kind: RelationKind,
        to_kind: RelationKind,
        occurred_at: Timestamp,
    },

    /// Per-project settings changed (currently the Interview/Human Log
    /// auto-append toggles). Carries the full new state for replay.
    ProjectSettingsChanged {
        project_id: ProjectId,
        auto_append: taskagent_domain::AutoAppendSettings,
        at: Timestamp,
    },

    // ── Documents (PR1 §1-2) ──────────────────────────────────────────────────
    /// A new document was created. Emitted by `Command::CreateDocument` and
    /// also by `Command::CreateProject` for the two default documents
    /// (Interview, Human Log).
    DocumentCreated {
        document: Document,
    },

    /// The full markdown body was replaced.
    DocumentContentReplaced {
        document_id: DocumentId,
        content: String,
        at: Timestamp,
    },

    /// Markdown was appended to the existing body (the handler is free to
    /// inject a separator before `append`).
    DocumentContentAppended {
        document_id: DocumentId,
        append: String,
        at: Timestamp,
    },

    /// The document title was renamed.
    DocumentRenamed {
        document_id: DocumentId,
        title: String,
        at: Timestamp,
    },

    /// The document was soft-archived. Projector sets `archived_at`; the row
    /// remains queryable via `include_archived=true`.
    DocumentArchived {
        document_id: DocumentId,
        at: Timestamp,
    },
}

impl Event {
    /// Stable kind string for indexing and logging.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::TaskCreated { .. } => "task_created",
            Event::TaskUpdated { .. } => "task_updated",
            Event::TaskStatusChanged { .. } => "task_status_changed",
            Event::TaskPriorityChanged { .. } => "task_priority_changed",
            Event::TaskCompleted { .. } => "task_completed",
            Event::TaskDeleted { .. } => "task_deleted",
            Event::TaskSplitGenerated { .. } => "task_split_generated",
            Event::ConflictResolved { .. } => "conflict_resolved",
            Event::ProjectCreated { .. } => "project_created",
            Event::ProjectUpdated { .. } => "project_updated",
            Event::ProjectSettingsChanged { .. } => "project_settings_changed",
            Event::ProjectDeleted { .. } => "project_deleted",
            Event::AgentActionRecorded { .. } => "agent_action_recorded",
            Event::CommentAdded { .. } => "comment_added",
            Event::CommentEdited { .. } => "comment_edited",
            Event::CommentDeleted { .. } => "comment_deleted",
            Event::TaskReopened { .. } => "task_reopened",
            Event::TaskClosed { .. } => "task_closed",
            Event::TaskCommented { .. } => "task_commented",
            // Plans
            Event::PlanCreated { .. } => "plan_created",
            Event::PlanUpdated { .. } => "plan_updated",
            Event::PlanStatusChanged { .. } => "plan_status_changed",
            Event::PlanGoalChanged { .. } => "plan_goal_changed",
            Event::PlanTaskAdded { .. } => "plan_task_added",
            Event::PlanTaskRemoved { .. } => "plan_task_removed",
            Event::PlanReordered { .. } => "plan_reordered",
            Event::PlanArchived { .. } => "plan_archived",
            // Runs
            Event::RunStarted { .. } => "run_started",
            Event::RunStepStarted { .. } => "run_step_started",
            Event::RunStepFinished { .. } => "run_step_finished",
            Event::RunCompleted { .. } => "run_completed",
            Event::RunFailed { .. } => "run_failed",
            Event::RunAborted { .. } => "run_aborted",
            Event::RunUnresponsive { .. } => "run_unresponsive",
            Event::RunStale { .. } => "run_stale",
            Event::RunNoteAppended { .. } => "run_note_appended",
            // Agent sessions
            Event::AgentSessionStarted { .. } => "agent_session_started",
            Event::AgentSessionEnded { .. } => "agent_session_ended",
            Event::AgentSessionPlanUpdated { .. } => "agent_session_plan_updated",
            Event::SessionArtifactAttached { .. } => "session_artifact_attached",
            Event::AgentClaimed { .. } => "agent_claimed",
            Event::AgentReleased { .. } => "agent_released",
            Event::FilesReserved { .. } => "files_reserved",
            Event::FilesReleased { .. } => "files_released",
            // Semantic
            Event::PlanModifiedByHuman { .. } => "plan_modified_by_human",
            Event::TaskContested { .. } => "task_contested",
            Event::RunObsolescedByPlanEdit { .. } => "run_obsolesced_by_plan_edit",
            // Run signals
            Event::RunStopRequested { .. } => "run_stop_requested",
            Event::RunElicitationRequested { .. } => "run_elicitation_requested",
            Event::RunAuthRequired { .. } => "run_auth_required",
            Event::RunInterventionAccepted { .. } => "run_intervention_accepted",
            // Task relations
            Event::TaskLinked { .. } => "task.linked",
            Event::TaskUnlinked { .. } => "task.unlinked",
            Event::TaskUnblocked { .. } => "task.unblocked",
            Event::TaskDueElapsed { .. } => "task.due",
            Event::TaskRelationKindChanged { .. } => "task.relation_kind_changed",
            // Documents (PR1)
            Event::DocumentCreated { .. } => "document_created",
            Event::DocumentContentReplaced { .. } => "document_content_replaced",
            Event::DocumentContentAppended { .. } => "document_content_appended",
            Event::DocumentRenamed { .. } => "document_renamed",
            Event::DocumentArchived { .. } => "document_archived",
        }
    }

    /// The task this event targets, if any.
    pub fn target_task(&self) -> Option<TaskId> {
        match self {
            Event::TaskCreated { task } => task.id,
            Event::TaskUpdated { task_id, .. }
            | Event::TaskStatusChanged { task_id, .. }
            | Event::TaskPriorityChanged { task_id, .. }
            | Event::TaskCompleted { task_id, .. }
            | Event::TaskDeleted { task_id } => Some(*task_id),
            Event::TaskSplitGenerated { parent, .. } => Some(*parent),
            Event::ConflictResolved { .. } => None,
            Event::CommentAdded { comment } => Some(comment.task_id),
            Event::CommentEdited { task_id, .. } => Some(*task_id),
            Event::CommentDeleted { task_id, .. } => Some(*task_id),
            Event::TaskReopened { task_id, .. }
            | Event::TaskClosed { task_id, .. }
            | Event::TaskCommented { task_id, .. } => Some(*task_id),
            // Plan task events carry the task id.
            Event::PlanTaskAdded { task_id, .. } | Event::PlanTaskRemoved { task_id, .. } => {
                Some(*task_id)
            }
            // Step events carry the task being executed.
            Event::RunStepStarted { task_id, .. } | Event::RunStepFinished { task_id, .. } => {
                Some(*task_id)
            }
            // Claim events are per-task.
            Event::AgentClaimed { task_id, .. } | Event::AgentReleased { task_id, .. } => {
                Some(*task_id)
            }
            // Lease events are per-task (FilesReserved carries it via its leases).
            Event::FilesReleased { task_id, .. } => Some(*task_id),
            Event::FilesReserved { leases } => leases.first().map(|l| l.task_id),
            // Contested task.
            Event::TaskContested { task_id, .. } => Some(*task_id),
            // Task relation events — target is the `from` endpoint (the relation's source).
            Event::TaskLinked { from, .. }
            | Event::TaskUnlinked { from, .. }
            | Event::TaskRelationKindChanged { from, .. } => Some(*from),
            Event::TaskUnblocked { task_id, .. } => Some(*task_id),
            Event::TaskDueElapsed { task_id, .. } => Some(*task_id),
            // All remaining plan/run/session/signal events do not resolve to a single task.
            _ => None,
        }
    }

    /// The project this event targets *if the project id is carried in the
    /// payload itself*. Events that only carry a `task_id` (e.g.
    /// `TaskStatusChanged`) return `None` — callers that need to filter by
    /// project must resolve the task → project mapping asynchronously
    /// (the WS handler does this against `TaskRepo`).
    pub fn target_project(&self) -> Option<ProjectId> {
        match self {
            Event::TaskCreated { task } => task.project_id,
            Event::ConflictResolved { .. } => None,
            Event::ProjectCreated { project } => Some(project.id),
            Event::ProjectUpdated { project_id, .. } => Some(*project_id),
            Event::ProjectSettingsChanged { project_id, .. } => Some(*project_id),
            Event::ProjectDeleted { project_id } => Some(*project_id),
            // Plans carry their project id inline.
            Event::PlanCreated { plan } => Some(plan.project_id),
            // Documents carry project_id inline on creation; other Document*
            // variants resolve project via the document repo.
            Event::DocumentCreated { document } => Some(document.project_id),
            // All other events do not carry project_id inline.
            _ => None,
        }
    }

    /// Channel classification used by WS / inbox / webhook subscribers.
    /// See [`Channel`] for the canonical enum.
    pub fn channel(&self) -> Channel {
        match self {
            // ── Tasks channel ─────────────────────────────────────────────────
            Event::TaskCreated { .. }
            | Event::TaskUpdated { .. }
            | Event::TaskStatusChanged { .. }
            | Event::TaskPriorityChanged { .. }
            | Event::TaskCompleted { .. }
            | Event::TaskDeleted { .. }
            | Event::TaskSplitGenerated { .. }
            | Event::ConflictResolved { .. }
            | Event::TaskReopened { .. }
            | Event::TaskClosed { .. }
            | Event::ProjectCreated { .. }
            | Event::ProjectUpdated { .. }
            | Event::ProjectSettingsChanged { .. }
            | Event::ProjectDeleted { .. }
            | Event::TaskLinked { .. }
            | Event::TaskUnlinked { .. }
            | Event::TaskUnblocked { .. }
            | Event::TaskDueElapsed { .. }
            | Event::TaskRelationKindChanged { .. } => Channel::Tasks,

            // ── Comments channel ──────────────────────────────────────────────
            Event::CommentAdded { .. }
            | Event::CommentEdited { .. }
            | Event::CommentDeleted { .. }
            | Event::TaskCommented { .. } => Channel::Comments,

            // ── AgentStatus channel ───────────────────────────────────────────
            Event::AgentActionRecorded { .. } => Channel::AgentStatus,

            // ── Plans channel ─────────────────────────────────────────────────
            Event::PlanCreated { .. }
            | Event::PlanUpdated { .. }
            | Event::PlanStatusChanged { .. }
            | Event::PlanGoalChanged { .. }
            | Event::PlanTaskAdded { .. }
            | Event::PlanTaskRemoved { .. }
            | Event::PlanReordered { .. }
            | Event::PlanArchived { .. } => Channel::Plans,

            // ── Runs channel ──────────────────────────────────────────────────
            // Mechanical run events.
            Event::RunStarted { .. }
            | Event::RunStepStarted { .. }
            | Event::RunStepFinished { .. }
            | Event::RunCompleted { .. }
            | Event::RunFailed { .. }
            | Event::RunAborted { .. }
            // Liveness signals (§3.7.4).
            | Event::RunUnresponsive { .. }
            | Event::RunStale { .. }
            // Run notes (§3.8.2).
            | Event::RunNoteAppended { .. }
            // Agent session events.
            | Event::AgentSessionStarted { .. }
            | Event::AgentSessionEnded { .. }
            | Event::AgentSessionPlanUpdated { .. }
            | Event::SessionArtifactAttached { .. }
            | Event::AgentClaimed { .. }
            | Event::AgentReleased { .. }
            | Event::FilesReserved { .. }
            | Event::FilesReleased { .. }
            // Semantic events — signals for the agent, not the task feed.
            | Event::PlanModifiedByHuman { .. }
            | Event::TaskContested { .. }
            | Event::RunObsolescedByPlanEdit { .. }
            // Run signals (Linear B.5).
            | Event::RunStopRequested { .. }
            | Event::RunElicitationRequested { .. }
            | Event::RunAuthRequired { .. }
            | Event::RunInterventionAccepted { .. } => Channel::Runs,

            // ── Documents channel (PR1) ───────────────────────────────────────
            Event::DocumentCreated { .. }
            | Event::DocumentContentReplaced { .. }
            | Event::DocumentContentAppended { .. }
            | Event::DocumentRenamed { .. }
            | Event::DocumentArchived { .. } => Channel::Documents,
        }
    }
}

/// Channel classification used by realtime subscribers (WS, agent inbox,
/// webhooks). One event always maps to exactly one channel; subscribers
/// pick which channels they want from the [`Subscribe`](crate) message.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// Task lifecycle and metadata events.
    Tasks,
    /// Comment events (raw + semantic `TaskCommented`).
    Comments,
    /// Agent action events.
    AgentStatus,
    /// Agent presence (start/end of session) — populated when sessions land.
    Presence,
    /// Webhook delivery events.
    Webhooks,
    /// Plan lifecycle and mutation events.
    Plans,
    /// Run lifecycle, agent-session, and signal events.
    Runs,
    /// Document lifecycle and content events (PR1).
    Documents,
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use taskagent_domain::{run::RunStatus, NewTask, SessionStepStatus};
    use taskagent_shared::{time, ProjectId};

    fn round_trip(ev: &Event) -> Event {
        let json = serde_json::to_string(ev).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    fn assert_round_trip(ev: Event, expected_type: &str) {
        let json = serde_json::to_string(&ev).expect("serialize");
        assert!(
            json.contains(&format!("\"type\":\"{expected_type}\"")),
            "type tag missing in: {json}"
        );
        assert_eq!(
            round_trip(&ev),
            ev,
            "round-trip mismatch for {expected_type}"
        );
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    fn sample_plan() -> Plan {
        let now = time::now();
        Plan {
            id: PlanId::new(),
            project_id: ProjectId::new(),
            parent_plan_id: None,
            title: "Test plan".to_string(),
            description: String::new(),
            goal: "Get things done".to_string(),
            success_criteria: vec![],
            status: PlanStatus::Draft,
            owner: Actor::user(),
            created_at: now,
            updated_at: now,
            archived_at: None,
            source_brief: None,
        }
    }

    fn sample_run() -> Run {
        Run {
            id: RunId::new(),
            plan_id: PlanId::new(),
            agent_id: AgentId::new(),
            parent_run_id: None,
            started_at: time::now(),
            ended_at: None,
            status: RunStatus::Active,
            outcome: None,
            last_activity_at: None,
            unresponsive_at: None,
            stale_at: None,
        }
    }

    fn sample_session() -> AgentSession {
        AgentSession {
            id: AgentSessionId::new(),
            agent_id: AgentId::new(),
            parent_agent_id: None,
            started_at: time::now(),
            ended_at: None,
            plan_steps: vec![],
            metadata: serde_json::Value::Null,
        }
    }

    // ── existing variant smoke tests ──────────────────────────────────────────

    #[test]
    fn task_created_round_trip() {
        assert_round_trip(
            Event::TaskCreated {
                task: NewTask::new("hello"),
            },
            "task_created",
        );
    }

    // ── Plan events ───────────────────────────────────────────────────────────

    /// Regression: `project_created` events persisted before the `slug` field
    /// existed (pre-migration-0018) must still deserialise during event replay
    /// / workspace-graph catch-up. A missing `slug` previously aborted
    /// `load_since`, failing catch-up and 500ing every API route.
    #[test]
    fn project_created_without_slug_deserialises() {
        let legacy = r#"{
            "type": "project_created",
            "project": {
                "id": "00000000-0000-0000-0000-000000000001",
                "title": "Legacy",
                "description": null,
                "created_at": "2024-01-01T00:00:00Z",
                "updated_at": "2024-01-01T00:00:00Z"
            }
        }"#;
        let ev: Event = serde_json::from_str(legacy).expect("legacy payload must deserialise");
        match ev {
            Event::ProjectCreated { project } => assert_eq!(project.slug, ""),
            other => panic!("expected ProjectCreated, got {}", other.kind()),
        }
    }

    #[test]
    fn plan_created_round_trip() {
        assert_round_trip(
            Event::PlanCreated {
                plan: sample_plan(),
            },
            "plan_created",
        );
    }

    #[test]
    fn plan_updated_round_trip() {
        assert_round_trip(
            Event::PlanUpdated {
                plan_id: PlanId::new(),
                patch: PlanPatch {
                    title: Some("New title".into()),
                    description: None,
                    goal: None,
                    success_criteria: None,
                    parent_plan_id: None,
                },
            },
            "plan_updated",
        );
    }

    #[test]
    fn plan_status_changed_round_trip() {
        assert_round_trip(
            Event::PlanStatusChanged {
                plan_id: PlanId::new(),
                from: PlanStatus::Draft,
                to: PlanStatus::Active,
            },
            "plan_status_changed",
        );
    }

    #[test]
    fn plan_goal_changed_round_trip() {
        assert_round_trip(
            Event::PlanGoalChanged {
                plan_id: PlanId::new(),
                from: "old goal".into(),
                to: "new goal".into(),
            },
            "plan_goal_changed",
        );
    }

    #[test]
    fn plan_task_added_round_trip() {
        assert_round_trip(
            Event::PlanTaskAdded {
                plan_id: PlanId::new(),
                task_id: TaskId::new(),
                position: 0,
                depends_on: vec![],
            },
            "plan_task_added",
        );
    }

    #[test]
    fn plan_task_removed_round_trip() {
        assert_round_trip(
            Event::PlanTaskRemoved {
                plan_id: PlanId::new(),
                task_id: TaskId::new(),
            },
            "plan_task_removed",
        );
    }

    #[test]
    fn plan_reordered_round_trip() {
        assert_round_trip(
            Event::PlanReordered {
                plan_id: PlanId::new(),
                order: vec![TaskId::new(), TaskId::new()],
            },
            "plan_reordered",
        );
    }

    #[test]
    fn plan_archived_round_trip() {
        assert_round_trip(
            Event::PlanArchived {
                plan_id: PlanId::new(),
                at: time::now(),
            },
            "plan_archived",
        );
    }

    // ── Run events ────────────────────────────────────────────────────────────

    #[test]
    fn run_started_round_trip() {
        assert_round_trip(Event::RunStarted { run: sample_run() }, "run_started");
    }

    #[test]
    fn run_step_started_round_trip() {
        assert_round_trip(
            Event::RunStepStarted {
                run_id: RunId::new(),
                task_id: TaskId::new(),
                at: time::now(),
            },
            "run_step_started",
        );
    }

    #[test]
    fn run_step_finished_done_round_trip() {
        assert_round_trip(
            Event::RunStepFinished {
                run_id: RunId::new(),
                task_id: TaskId::new(),
                outcome: RunOutcome::Done,
                at: time::now(),
            },
            "run_step_finished",
        );
    }

    #[test]
    fn run_step_finished_failed_outcome_round_trip() {
        assert_round_trip(
            Event::RunStepFinished {
                run_id: RunId::new(),
                task_id: TaskId::new(),
                outcome: RunOutcome::Failed {
                    reason: "timed out".into(),
                },
                at: time::now(),
            },
            "run_step_finished",
        );
    }

    #[test]
    fn run_completed_round_trip() {
        assert_round_trip(
            Event::RunCompleted {
                run_id: RunId::new(),
                at: time::now(),
            },
            "run_completed",
        );
    }

    #[test]
    fn run_failed_round_trip() {
        assert_round_trip(
            Event::RunFailed {
                run_id: RunId::new(),
                reason: "internal error".into(),
                at: time::now(),
            },
            "run_failed",
        );
    }

    #[test]
    fn run_aborted_round_trip() {
        assert_round_trip(
            Event::RunAborted {
                run_id: RunId::new(),
                reason: "plan archived".into(),
                at: time::now(),
            },
            "run_aborted",
        );
    }

    #[test]
    fn run_note_appended_round_trip() {
        use taskagent_shared::RunNoteId;
        assert_round_trip(
            Event::RunNoteAppended {
                run_id: RunId::new(),
                note_id: RunNoteId::new(),
                body: "first observation".into(),
                by_actor: Actor::user(),
                at: time::now(),
            },
            "run_note_appended",
        );
    }

    #[test]
    fn run_note_appended_channel_is_runs() {
        use taskagent_shared::RunNoteId;
        let ev = Event::RunNoteAppended {
            run_id: RunId::new(),
            note_id: RunNoteId::new(),
            body: "x".into(),
            by_actor: Actor::user(),
            at: time::now(),
        };
        assert_eq!(ev.channel(), Channel::Runs);
        assert_eq!(ev.kind(), "run_note_appended");
    }

    // ── Agent session events ──────────────────────────────────────────────────

    #[test]
    fn agent_session_started_round_trip() {
        assert_round_trip(
            Event::AgentSessionStarted {
                session: sample_session(),
            },
            "agent_session_started",
        );
    }

    #[test]
    fn agent_session_started_with_steps_round_trip() {
        let session = AgentSession {
            plan_steps: vec![
                AgentSessionPlanStep {
                    content: "step 1".into(),
                    status: SessionStepStatus::Pending,
                },
                AgentSessionPlanStep {
                    content: "step 2".into(),
                    status: SessionStepStatus::InProgress,
                },
            ],
            metadata: serde_json::json!({"model": "gpt-4"}),
            ..sample_session()
        };
        assert_round_trip(
            Event::AgentSessionStarted { session },
            "agent_session_started",
        );
    }

    #[test]
    fn agent_session_ended_round_trip() {
        assert_round_trip(
            Event::AgentSessionEnded {
                session_id: AgentSessionId::new(),
                at: time::now(),
            },
            "agent_session_ended",
        );
    }

    #[test]
    fn agent_session_plan_updated_round_trip() {
        assert_round_trip(
            Event::AgentSessionPlanUpdated {
                session_id: AgentSessionId::new(),
                steps: vec![
                    AgentSessionPlanStep {
                        content: "step 1".into(),
                        status: SessionStepStatus::Pending,
                    },
                    AgentSessionPlanStep {
                        content: "step 2".into(),
                        status: SessionStepStatus::Completed,
                    },
                ],
            },
            "agent_session_plan_updated",
        );
    }

    #[test]
    fn agent_claimed_round_trip() {
        assert_round_trip(
            Event::AgentClaimed {
                agent_id: AgentId::new(),
                task_id: TaskId::new(),
                expires_at: time::now(),
            },
            "agent_claimed",
        );
    }

    #[test]
    fn agent_released_round_trip() {
        assert_round_trip(
            Event::AgentReleased {
                agent_id: AgentId::new(),
                task_id: TaskId::new(),
            },
            "agent_released",
        );
    }

    // ── Semantic events ───────────────────────────────────────────────────────

    #[test]
    fn plan_modified_by_human_round_trip() {
        assert_round_trip(
            Event::PlanModifiedByHuman {
                plan_id: PlanId::new(),
                during_run_id: None,
            },
            "plan_modified_by_human",
        );
    }

    #[test]
    fn plan_modified_by_human_with_run_round_trip() {
        assert_round_trip(
            Event::PlanModifiedByHuman {
                plan_id: PlanId::new(),
                during_run_id: Some(RunId::new()),
            },
            "plan_modified_by_human",
        );
    }

    #[test]
    fn task_contested_round_trip() {
        assert_round_trip(
            Event::TaskContested {
                task_id: TaskId::new(),
                actors: vec![Actor::user(), Actor::user()],
                field: Some("status".into()),
            },
            "task_contested",
        );
    }

    #[test]
    fn run_obsolesced_by_plan_edit_round_trip() {
        for kind in [
            ObsolescenceKind::Archived,
            ObsolescenceKind::TaskRemoved,
            ObsolescenceKind::GoalChanged,
        ] {
            assert_round_trip(
                Event::RunObsolescedByPlanEdit {
                    run_id: RunId::new(),
                    plan_id: PlanId::new(),
                    kind,
                },
                "run_obsolesced_by_plan_edit",
            );
        }
    }

    // ── Run signal events (Linear B.5) ────────────────────────────────────────

    #[test]
    fn run_stop_requested_round_trip() {
        assert_round_trip(
            Event::RunStopRequested {
                run_id: RunId::new(),
                reason: Some("user request".into()),
                by: Actor::user(),
            },
            "run_stop_requested",
        );
    }

    #[test]
    fn run_stop_requested_no_reason_round_trip() {
        assert_round_trip(
            Event::RunStopRequested {
                run_id: RunId::new(),
                reason: None,
                by: Actor::user(),
            },
            "run_stop_requested",
        );
    }

    #[test]
    fn run_elicitation_requested_round_trip() {
        assert_round_trip(
            Event::RunElicitationRequested {
                run_id: RunId::new(),
                prompt: "Which approach?".into(),
                choices: vec!["A".into(), "B".into()],
            },
            "run_elicitation_requested",
        );
    }

    #[test]
    fn run_auth_required_round_trip() {
        assert_round_trip(
            Event::RunAuthRequired {
                run_id: RunId::new(),
                scope: "github:read".into(),
            },
            "run_auth_required",
        );
    }

    #[test]
    fn run_intervention_accepted_round_trip() {
        assert_round_trip(
            Event::RunInterventionAccepted {
                run_id: RunId::new(),
                choice: "A".into(),
                by: Actor::user(),
            },
            "run_intervention_accepted",
        );
    }

    // ── Channel classification ────────────────────────────────────────────────

    #[test]
    fn channel_plans_variants() {
        let plan_id = PlanId::new();
        let task_id = TaskId::new();
        let events = vec![
            Event::PlanCreated {
                plan: sample_plan(),
            },
            Event::PlanUpdated {
                plan_id,
                patch: PlanPatch::default(),
            },
            Event::PlanStatusChanged {
                plan_id,
                from: PlanStatus::Draft,
                to: PlanStatus::Active,
            },
            Event::PlanGoalChanged {
                plan_id,
                from: "a".into(),
                to: "b".into(),
            },
            Event::PlanTaskAdded {
                plan_id,
                task_id,
                position: 0,
                depends_on: vec![],
            },
            Event::PlanTaskRemoved { plan_id, task_id },
            Event::PlanReordered {
                plan_id,
                order: vec![],
            },
            Event::PlanArchived {
                plan_id,
                at: time::now(),
            },
        ];
        for ev in &events {
            assert_eq!(
                ev.channel(),
                Channel::Plans,
                "expected Plans for {}",
                ev.kind()
            );
        }
    }

    #[test]
    fn channel_runs_variants() {
        let run_id = RunId::new();
        let plan_id = PlanId::new();
        let task_id = TaskId::new();
        let events = vec![
            Event::RunStarted { run: sample_run() },
            Event::RunStepStarted {
                run_id,
                task_id,
                at: time::now(),
            },
            Event::RunStepFinished {
                run_id,
                task_id,
                outcome: RunOutcome::Done,
                at: time::now(),
            },
            Event::RunCompleted {
                run_id,
                at: time::now(),
            },
            Event::RunFailed {
                run_id,
                reason: "e".into(),
                at: time::now(),
            },
            Event::RunAborted {
                run_id,
                reason: "e".into(),
                at: time::now(),
            },
            Event::AgentSessionStarted {
                session: sample_session(),
            },
            Event::AgentSessionEnded {
                session_id: AgentSessionId::new(),
                at: time::now(),
            },
            Event::AgentSessionPlanUpdated {
                session_id: AgentSessionId::new(),
                steps: vec![],
            },
            Event::AgentClaimed {
                agent_id: AgentId::new(),
                task_id,
                expires_at: time::now(),
            },
            Event::AgentReleased {
                agent_id: AgentId::new(),
                task_id,
            },
            Event::PlanModifiedByHuman {
                plan_id,
                during_run_id: None,
            },
            Event::TaskContested {
                task_id,
                actors: vec![],
                field: None,
            },
            Event::RunObsolescedByPlanEdit {
                run_id,
                plan_id,
                kind: ObsolescenceKind::Archived,
            },
            Event::RunStopRequested {
                run_id,
                reason: None,
                by: Actor::user(),
            },
            Event::RunElicitationRequested {
                run_id,
                prompt: "p".into(),
                choices: vec![],
            },
            Event::RunAuthRequired {
                run_id,
                scope: "s".into(),
            },
            Event::RunInterventionAccepted {
                run_id,
                choice: "c".into(),
                by: Actor::user(),
            },
        ];
        for ev in &events {
            assert_eq!(
                ev.channel(),
                Channel::Runs,
                "expected Runs for {}",
                ev.kind()
            );
        }
    }

    // ── Task relation events (§3.2 W1.3) ─────────────────────────────────────

    #[test]
    fn task_linked_round_trip() {
        use taskagent_domain::{Actor, RelationKind};
        use taskagent_shared::RelationId;
        assert_round_trip(
            Event::TaskLinked {
                relation_id: RelationId::new(),
                from: TaskId::new(),
                to: TaskId::new(),
                kind: RelationKind::Blocks,
                actor: Actor::user(),
                occurred_at: time::now(),
            },
            "task_linked",
        );
    }

    #[test]
    fn task_unlinked_round_trip() {
        use taskagent_domain::RelationKind;
        use taskagent_shared::RelationId;
        assert_round_trip(
            Event::TaskUnlinked {
                relation_id: RelationId::new(),
                from: TaskId::new(),
                to: TaskId::new(),
                kind: RelationKind::RelatesTo,
                occurred_at: time::now(),
            },
            "task_unlinked",
        );
    }

    #[test]
    fn task_unblocked_round_trip() {
        assert_round_trip(
            Event::TaskUnblocked {
                task_id: TaskId::new(),
                unblocked_by: TaskId::new(),
                occurred_at: time::now(),
            },
            "task_unblocked",
        );
    }

    #[test]
    fn task_relation_kind_changed_round_trip() {
        use taskagent_domain::RelationKind;
        use taskagent_shared::RelationId;
        assert_round_trip(
            Event::TaskRelationKindChanged {
                relation_id: RelationId::new(),
                from: TaskId::new(),
                to: TaskId::new(),
                from_kind: RelationKind::Blocks,
                to_kind: RelationKind::WasBlocking,
                occurred_at: time::now(),
            },
            "task_relation_kind_changed",
        );
    }

    #[test]
    fn relation_event_kind_strings() {
        use taskagent_domain::{Actor, RelationKind};
        use taskagent_shared::RelationId;
        let linked = Event::TaskLinked {
            relation_id: RelationId::new(),
            from: TaskId::new(),
            to: TaskId::new(),
            kind: RelationKind::Blocks,
            actor: Actor::user(),
            occurred_at: time::now(),
        };
        let unlinked = Event::TaskUnlinked {
            relation_id: RelationId::new(),
            from: TaskId::new(),
            to: TaskId::new(),
            kind: RelationKind::Duplicates,
            occurred_at: time::now(),
        };
        let unblocked = Event::TaskUnblocked {
            task_id: TaskId::new(),
            unblocked_by: TaskId::new(),
            occurred_at: time::now(),
        };
        assert_eq!(linked.kind(), "task.linked");
        assert_eq!(unlinked.kind(), "task.unlinked");
        assert_eq!(unblocked.kind(), "task.unblocked");
    }

    #[test]
    fn relation_events_channel_is_tasks() {
        use taskagent_domain::{Actor, RelationKind};
        use taskagent_shared::RelationId;
        let from = TaskId::new();
        let linked = Event::TaskLinked {
            relation_id: RelationId::new(),
            from,
            to: TaskId::new(),
            kind: RelationKind::Blocks,
            actor: Actor::user(),
            occurred_at: time::now(),
        };
        let unlinked = Event::TaskUnlinked {
            relation_id: RelationId::new(),
            from,
            to: TaskId::new(),
            kind: RelationKind::Blocks,
            occurred_at: time::now(),
        };
        let unblocked = Event::TaskUnblocked {
            task_id: TaskId::new(),
            unblocked_by: TaskId::new(),
            occurred_at: time::now(),
        };
        assert_eq!(linked.channel(), Channel::Tasks);
        assert_eq!(unlinked.channel(), Channel::Tasks);
        assert_eq!(unblocked.channel(), Channel::Tasks);
    }

    #[test]
    fn relation_events_target_task() {
        use taskagent_domain::{Actor, RelationKind};
        use taskagent_shared::RelationId;
        let from = TaskId::new();
        let task_id = TaskId::new();

        let linked = Event::TaskLinked {
            relation_id: RelationId::new(),
            from,
            to: TaskId::new(),
            kind: RelationKind::Blocks,
            actor: Actor::user(),
            occurred_at: time::now(),
        };
        assert_eq!(linked.target_task(), Some(from));

        let unlinked = Event::TaskUnlinked {
            relation_id: RelationId::new(),
            from,
            to: TaskId::new(),
            kind: RelationKind::Blocks,
            occurred_at: time::now(),
        };
        assert_eq!(unlinked.target_task(), Some(from));

        let unblocked = Event::TaskUnblocked {
            task_id,
            unblocked_by: TaskId::new(),
            occurred_at: time::now(),
        };
        assert_eq!(unblocked.target_task(), Some(task_id));
    }

    // ── Document events (PR1 §2) ─────────────────────────────────────────────

    fn sample_document() -> taskagent_domain::Document {
        use taskagent_domain::{Document, DocumentKind};
        use taskagent_shared::DocumentId;
        let now = time::now();
        Document {
            id: DocumentId::new(),
            project_id: ProjectId::new(),
            kind: DocumentKind::Interview,
            title: "Interview".into(),
            content: String::new(),
            created_at: now,
            updated_at: now,
            archived_at: None,
        }
    }

    #[test]
    fn document_created_round_trip() {
        assert_round_trip(
            Event::DocumentCreated {
                document: sample_document(),
            },
            "document_created",
        );
    }

    #[test]
    fn document_content_replaced_round_trip() {
        use taskagent_shared::DocumentId;
        assert_round_trip(
            Event::DocumentContentReplaced {
                document_id: DocumentId::new(),
                content: "# New body".into(),
                at: time::now(),
            },
            "document_content_replaced",
        );
    }

    #[test]
    fn document_content_appended_round_trip() {
        use taskagent_shared::DocumentId;
        assert_round_trip(
            Event::DocumentContentAppended {
                document_id: DocumentId::new(),
                append: "another paragraph".into(),
                at: time::now(),
            },
            "document_content_appended",
        );
    }

    #[test]
    fn document_renamed_round_trip() {
        use taskagent_shared::DocumentId;
        assert_round_trip(
            Event::DocumentRenamed {
                document_id: DocumentId::new(),
                title: "Renamed".into(),
                at: time::now(),
            },
            "document_renamed",
        );
    }

    #[test]
    fn document_archived_round_trip() {
        use taskagent_shared::DocumentId;
        assert_round_trip(
            Event::DocumentArchived {
                document_id: DocumentId::new(),
                at: time::now(),
            },
            "document_archived",
        );
    }

    #[test]
    fn document_created_target_project() {
        let doc = sample_document();
        let project_id = doc.project_id;
        let ev = Event::DocumentCreated { document: doc };
        assert_eq!(ev.target_project(), Some(project_id));
    }

    #[test]
    fn document_non_created_target_project_is_none() {
        use taskagent_shared::DocumentId;
        let evs = vec![
            Event::DocumentContentReplaced {
                document_id: DocumentId::new(),
                content: "x".into(),
                at: time::now(),
            },
            Event::DocumentContentAppended {
                document_id: DocumentId::new(),
                append: "x".into(),
                at: time::now(),
            },
            Event::DocumentRenamed {
                document_id: DocumentId::new(),
                title: "x".into(),
                at: time::now(),
            },
            Event::DocumentArchived {
                document_id: DocumentId::new(),
                at: time::now(),
            },
        ];
        for ev in &evs {
            assert_eq!(ev.target_project(), None, "expected None for {}", ev.kind());
        }
    }

    #[test]
    fn channel_documents_variants() {
        use taskagent_shared::DocumentId;
        let doc_id = DocumentId::new();
        let events = vec![
            Event::DocumentCreated {
                document: sample_document(),
            },
            Event::DocumentContentReplaced {
                document_id: doc_id,
                content: "c".into(),
                at: time::now(),
            },
            Event::DocumentContentAppended {
                document_id: doc_id,
                append: "a".into(),
                at: time::now(),
            },
            Event::DocumentRenamed {
                document_id: doc_id,
                title: "t".into(),
                at: time::now(),
            },
            Event::DocumentArchived {
                document_id: doc_id,
                at: time::now(),
            },
        ];
        for ev in &events {
            assert_eq!(
                ev.channel(),
                Channel::Documents,
                "expected Documents for {}",
                ev.kind()
            );
        }
    }

    #[test]
    fn document_event_kind_strings() {
        use taskagent_shared::DocumentId;
        assert_eq!(
            Event::DocumentCreated {
                document: sample_document(),
            }
            .kind(),
            "document_created"
        );
        assert_eq!(
            Event::DocumentContentReplaced {
                document_id: DocumentId::new(),
                content: String::new(),
                at: time::now(),
            }
            .kind(),
            "document_content_replaced"
        );
        assert_eq!(
            Event::DocumentContentAppended {
                document_id: DocumentId::new(),
                append: String::new(),
                at: time::now(),
            }
            .kind(),
            "document_content_appended"
        );
        assert_eq!(
            Event::DocumentRenamed {
                document_id: DocumentId::new(),
                title: String::new(),
                at: time::now(),
            }
            .kind(),
            "document_renamed"
        );
        assert_eq!(
            Event::DocumentArchived {
                document_id: DocumentId::new(),
                at: time::now(),
            }
            .kind(),
            "document_archived"
        );
    }

    // ── kind() exhaustiveness spot-check ─────────────────────────────────────

    #[test]
    fn kind_strings_are_snake_case_and_nonempty() {
        let events = vec![
            Event::PlanCreated {
                plan: sample_plan(),
            },
            Event::RunStarted { run: sample_run() },
            Event::AgentSessionStarted {
                session: sample_session(),
            },
            Event::RunObsolescedByPlanEdit {
                run_id: RunId::new(),
                plan_id: PlanId::new(),
                kind: ObsolescenceKind::TaskRemoved,
            },
            Event::RunInterventionAccepted {
                run_id: RunId::new(),
                choice: "x".into(),
                by: Actor::user(),
            },
        ];
        for ev in &events {
            let k = ev.kind();
            assert!(!k.is_empty());
            assert!(
                k.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "not snake_case: {k}"
            );
        }
    }
}
