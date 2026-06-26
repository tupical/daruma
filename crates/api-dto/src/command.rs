//! `Command` enum and `CommandEnvelope` — the canonical mutation wire format.
//!
//! Previously defined in `crates/core/src/command.rs`. Moved here so both
//! the server and the WASM frontend can share the same types without pulling
//! in tokio / sqlx.

use serde::{Deserialize, Serialize};
use daruma_domain::{
    AgentAction, AgentSessionPlanStep, CommentPatch, CompletionNote, NewComment, NewDocument,
    NewPlan, NewTask, PlanPatch, PlanStatus, Priority, RelationKind, RunOutcome,
    SessionArtifactKind, SignalKind, Status, TaskPatch, WorkLease,
};
use daruma_shared::{
    AgentId, AgentSessionId, CommentId, DocumentId, PlanId, ProjectId, RelationId, RuleId, RunId,
    TaskId, WorkUnitId,
};

/// All mutations are commands. Tagged-union JSON for stable wire format.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    // ── Task commands ─────────────────────────────────────────────────────────
    CreateTask {
        task: NewTask,
    },
    UpdateTask {
        id: TaskId,
        patch: TaskPatch,
    },
    CompleteTask {
        id: TaskId,
        /// Optional completion note (reason / result summary / acceptance
        /// status / artifacts). Omitted by legacy clients — `CompleteTask`
        /// stays backward compatible: no note means the same single-arg
        /// command as before. The handler stamps the completing actor onto
        /// the note so human-verified and agent-self-reported completions are
        /// distinguishable in the audit trail.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<CompletionNote>,
    },
    DeleteTask {
        id: TaskId,
    },
    SetStatus {
        id: TaskId,
        status: Status,
        /// Allow transition into `in_progress` even when `can_start` reports
        /// active blockers. Omitted by legacy clients; defaults to soft warning.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        force: bool,
    },
    SetPriority {
        id: TaskId,
        priority: Priority,
    },

    // ── Bulk task commands (§3.7.7 / LIN B.7) ─────────────────────────────────
    /// Atomically set the same status on up to 50 tasks. Duplicate ids are
    /// deduped; fail-fast if any id is missing. Emits one `TaskStatusChanged`
    /// (plus the usual side-effect events) per task that actually transitions.
    BulkSetStatus {
        ids: Vec<TaskId>,
        status: Status,
    },

    /// Atomically attach up to 50 tasks to a single plan. Duplicate ids are
    /// deduped; fail-fast if any id (or the plan) is missing. Emits one
    /// `PlanTaskAdded` per task plus a single `PlanModifiedByHuman`.
    BulkAttachToPlan {
        plan_id: PlanId,
        task_ids: Vec<TaskId>,
    },

    // ── Project commands ──────────────────────────────────────────────────────
    CreateProject {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    CreateWorkUnit {
        work_unit: daruma_domain::NewWorkUnit,
    },
    CompleteWorkUnit {
        id: WorkUnitId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<String>,
        #[serde(default)]
        produced_artifacts: Vec<String>,
    },
    ReleaseWorkUnit {
        id: WorkUnitId,
    },
    SetWorkUnitStatus {
        id: WorkUnitId,
        status: daruma_domain::WorkUnitStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    UpdateProjectSettings {
        project_id: ProjectId,
        #[serde(default)]
        auto_append: daruma_domain::AutoAppendPatch,
    },
    UpdateProject {
        id: ProjectId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<Option<String>>,
    },
    /// Delete a project.  Handler rejects the command unless the project has
    /// zero tasks and zero plans.  No cascading delete is performed.
    DeleteProject {
        id: ProjectId,
    },
    SplitTask {
        parent: TaskId,
        subtasks: Vec<NewTask>,
    },
    RecordAgentAction {
        action: AgentAction,
    },

    // ── Comment commands ──────────────────────────────────────────────────────
    AddComment {
        comment: NewComment,
    },
    EditComment {
        id: CommentId,
        patch: CommentPatch,
    },
    DeleteComment {
        id: CommentId,
    },

    // ── Plan commands (W2.2) ──────────────────────────────────────────────────
    /// Create a new plan.  `external_ref` enables idempotent creation from an
    /// external system: `(tenant, kind, external_id)`.
    CreatePlan {
        plan: NewPlan,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        external_ref: Option<(String, String, String)>,
    },

    /// Update title / description / goal / success_criteria via a sparse patch.
    UpdatePlan {
        id: PlanId,
        patch: PlanPatch,
    },

    /// Archive the plan (no further runs allowed).  Atomically aborts all
    /// active runs.
    ArchivePlan {
        id: PlanId,
    },

    /// Attach a task to a plan at the given position (append at end if absent).
    AddPlanTask {
        plan_id: PlanId,
        task_id: TaskId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        position: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        depends_on: Option<Vec<TaskId>>,
    },

    /// Detach a task from a plan.  If the task is currently being executed,
    /// emits `TaskContested + RunStepFinished{Superseded}` atomically.
    RemovePlanTask {
        plan_id: PlanId,
        task_id: TaskId,
    },

    /// Replace the entire task order within a plan.
    ReorderPlan {
        plan_id: PlanId,
        order: Vec<TaskId>,
    },

    /// Replace the plan's goal text.
    SetPlanGoal {
        plan_id: PlanId,
        goal: String,
    },

    /// Transition the plan's lifecycle status.
    SetPlanStatus {
        plan_id: PlanId,
        status: PlanStatus,
    },

    // ── Run commands (W2.2) ───────────────────────────────────────────────────
    /// Start a new run of a plan by an agent.
    StartRun {
        plan_id: PlanId,
        agent_id: AgentId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_run_id: Option<RunId>,
    },

    /// Mark the beginning of a task step within a run.
    RunStartStep {
        run_id: RunId,
        task_id: TaskId,
    },

    /// Mark the completion of a task step.
    RunFinishStep {
        run_id: RunId,
        task_id: TaskId,
        outcome: RunOutcome,
    },

    /// Terminate a run successfully.
    CompleteRun {
        run_id: RunId,
    },

    /// Terminate a run with a failure.
    FailRun {
        run_id: RunId,
        reason: String,
    },

    /// Abort a run (e.g. plan archived or explicit stop).
    AbortRun {
        run_id: RunId,
        reason: String,
    },

    /// §3.8.2 — append a free-form journal entry to an active run. The actor
    /// is taken from the envelope; body is required (non-empty, ≤ 4 KiB).
    AppendRunNote {
        run_id: RunId,
        body: String,
    },

    // ── Agent session commands (W2.2) ─────────────────────────────────────────
    /// Start a new agent session (Linear B.1).
    StartAgentSession {
        agent_id: AgentId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_agent_id: Option<AgentId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },

    /// End an agent session.
    EndAgentSession {
        id: AgentSessionId,
    },

    /// Replace the session's plan-steps list (Linear B.1).  Maximum 100 steps.
    UpdateAgentSessionPlan {
        id: AgentSessionId,
        steps: Vec<AgentSessionPlanStep>,
    },

    /// Attach a file/url/diff artifact reference to an agent session.
    AttachSessionArtifact {
        session_id: AgentSessionId,
        kind: SessionArtifactKind,
        reference: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },

    // ── Run signal commands — Linear B.5 (W2.2) ───────────────────────────────
    /// Send a typed signal to a run (Stop / Elicit / AuthRequired).
    SendRunSignal {
        run_id: RunId,
        kind: SignalKind,
    },

    /// Human responds to an elicitation request.
    RespondRunSignal {
        run_id: RunId,
        choice: String,
    },

    // ── Relation commands (§3.2 W2.1) ────────────────────────────────────────
    /// Create a typed relation between two tasks.
    LinkTasks {
        from: TaskId,
        to: TaskId,
        kind: RelationKind,
    },

    /// Remove a typed relation by its id.
    UnlinkTasks {
        id: RelationId,
    },

    // ── Claim commands (W2.2) ─────────────────────────────────────────────────
    /// Acquire an optimistic claim on a task (TTL in seconds).
    AcquireClaim {
        agent_id: AgentId,
        task_id: TaskId,
        ttl_secs: u32,
    },

    /// Release a previously-acquired claim.
    ReleaseClaim {
        agent_id: AgentId,
        task_id: TaskId,
    },

    /// Record file/path leases reserved for a task (audit + WS projection).
    /// The atomic reservation already happened in the repo; this re-applies it
    /// idempotently through the event log.
    ReserveFiles {
        leases: Vec<WorkLease>,
    },

    /// Release all file/path leases held by an agent for a task.
    ReleaseFiles {
        agent_id: AgentId,
        task_id: TaskId,
    },

    // ── Document commands (PR1 §5) ────────────────────────────────────────────
    /// Create a new markdown document attached to a project.
    CreateDocument {
        new_doc: NewDocument,
    },

    /// Replace the full markdown body of an existing document.
    ReplaceDocumentContent {
        document_id: DocumentId,
        content: String,
    },

    /// Append a chunk of markdown to an existing document's body.
    AppendDocumentContent {
        document_id: DocumentId,
        append: String,
    },

    /// Rename an existing document.
    RenameDocument {
        document_id: DocumentId,
        title: String,
    },

    /// Soft-archive an existing document.
    ArchiveDocument {
        document_id: DocumentId,
    },

    // ── Lifecycle rules (docs/LIFECYCLE_RULES_SPEC.md §4) ─────────────────────
    /// Create a lifecycle rule. Rejected if a rule with the same `rule_key`
    /// already exists at the same scope level.
    CreateRule {
        rule: daruma_domain::NewRule,
    },

    /// Patch an existing lifecycle rule (mode/condition/requirement/…).
    UpdateRule {
        id: RuleId,
        patch: daruma_domain::RulePatch,
    },

    /// Disable a lifecycle rule (`enabled=false`). A disabled rule is not
    /// evaluated by the gate.
    DisableRule {
        id: RuleId,
    },

    // ── Evidence registry (OSS task 019eb65a-3185; spec §1.3) ─────────────────
    /// Record a piece of evidence (immutable). When `supersedes` is set the
    /// older record is marked superseded (not edited). Evidence is what
    /// satisfies a `required` rule's requirement at the lifecycle gate.
    RecordEvidence {
        evidence: daruma_domain::NewEvidence,
    },
}

impl Command {
    /// Stable kind string for indexing, logging and metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            Command::CreateTask { .. } => "create_task",
            Command::UpdateTask { .. } => "update_task",
            Command::CompleteTask { .. } => "complete_task",
            Command::DeleteTask { .. } => "delete_task",
            Command::SetStatus { .. } => "set_status",
            Command::SetPriority { .. } => "set_priority",
            Command::BulkSetStatus { .. } => "bulk_set_status",
            Command::BulkAttachToPlan { .. } => "bulk_attach_to_plan",
            Command::CreateProject { .. } => "create_project",
            Command::UpdateProject { .. } => "update_project",
            Command::UpdateProjectSettings { .. } => "update_project_settings",
            Command::CreateWorkUnit { .. } => "create_work_unit",
            Command::CompleteWorkUnit { .. } => "complete_work_unit",
            Command::ReleaseWorkUnit { .. } => "release_work_unit",
            Command::SetWorkUnitStatus { .. } => "set_work_unit_status",
            Command::DeleteProject { .. } => "delete_project",
            Command::SplitTask { .. } => "split_task",
            Command::RecordAgentAction { .. } => "record_agent_action",
            Command::AddComment { .. } => "add_comment",
            Command::EditComment { .. } => "edit_comment",
            Command::DeleteComment { .. } => "delete_comment",
            // Plans
            Command::CreatePlan { .. } => "create_plan",
            Command::UpdatePlan { .. } => "update_plan",
            Command::ArchivePlan { .. } => "archive_plan",
            Command::AddPlanTask { .. } => "add_plan_task",
            Command::RemovePlanTask { .. } => "remove_plan_task",
            Command::ReorderPlan { .. } => "reorder_plan",
            Command::SetPlanGoal { .. } => "set_plan_goal",
            Command::SetPlanStatus { .. } => "set_plan_status",
            // Runs
            Command::StartRun { .. } => "start_run",
            Command::RunStartStep { .. } => "run_start_step",
            Command::RunFinishStep { .. } => "run_finish_step",
            Command::CompleteRun { .. } => "complete_run",
            Command::FailRun { .. } => "fail_run",
            Command::AbortRun { .. } => "abort_run",
            Command::AppendRunNote { .. } => "append_run_note",
            // Sessions
            Command::StartAgentSession { .. } => "start_agent_session",
            Command::EndAgentSession { .. } => "end_agent_session",
            Command::UpdateAgentSessionPlan { .. } => "update_agent_session_plan",
            Command::AttachSessionArtifact { .. } => "attach_session_artifact",
            // Signals
            Command::SendRunSignal { .. } => "send_run_signal",
            Command::RespondRunSignal { .. } => "respond_run_signal",
            // Relations
            Command::LinkTasks { .. } => "link_tasks",
            Command::UnlinkTasks { .. } => "unlink_tasks",
            // Claims
            Command::AcquireClaim { .. } => "acquire_claim",
            Command::ReleaseClaim { .. } => "release_claim",
            Command::ReserveFiles { .. } => "reserve_files",
            Command::ReleaseFiles { .. } => "release_files",
            // Documents (PR1)
            Command::CreateDocument { .. } => "create_document",
            Command::ReplaceDocumentContent { .. } => "replace_document_content",
            Command::AppendDocumentContent { .. } => "append_document_content",
            Command::RenameDocument { .. } => "rename_document",
            Command::ArchiveDocument { .. } => "archive_document",
            // Lifecycle rules
            Command::CreateRule { .. } => "create_rule",
            Command::UpdateRule { .. } => "update_rule",
            Command::DisableRule { .. } => "disable_rule",
            // Evidence registry
            Command::RecordEvidence { .. } => "record_evidence",
        }
    }
}

/// Convenience: a command together with the actor that issued it, and an
/// optional idempotency key (Linear A.1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub command: Command,
    #[serde(default)]
    pub actor: daruma_domain::Actor,
    /// Optional client-generated UUIDv4 for idempotent retry (Linear A.1).
    /// The dispatch layer checks `processed_command_ids` before forwarding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_command_id: Option<uuid::Uuid>,
}

impl CommandEnvelope {
    pub fn new(command: Command, actor: daruma_domain::Actor) -> Self {
        Self {
            command,
            actor,
            client_command_id: None,
        }
    }

    pub fn by_user(command: Command) -> Self {
        Self::new(command, daruma_domain::Actor::user())
    }

    pub fn by_agent(command: Command, agent_name: impl Into<String>) -> Self {
        Self::new(command, daruma_domain::Actor::agent(agent_name))
    }

    pub fn with_idempotency_key(mut self, key: uuid::Uuid) -> Self {
        self.client_command_id = Some(key);
        self
    }
}
