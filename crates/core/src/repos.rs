//! Repository traits for plan-domain aggregates.
//!
//! These traits are the contract between `taskagent-core` (commands,
//! concurrency logic) and `taskagent-storage` (concrete SQLite
//! implementations landing in W2.1).  Until W2.1 merges, the handler
//! scaffolds against these traits; tests supply in-memory stub impls.

use async_trait::async_trait;
use taskagent_domain::{AgentSession, Document, DocumentKind, Plan, PlanTask, Run, RunNote};
use taskagent_events::EventEnvelope;
use taskagent_shared::{
    AgentId, AgentSessionId, DocumentId, PlanId, ProjectId, Result, RunId, RunNoteId, TaskId,
};

// ── Plan ──────────────────────────────────────────────────────────────────────

/// Read / projection interface for the `plans` table.
#[async_trait]
pub trait PlanRepository: Send + Sync {
    /// Fetch a plan by id; `None` if not found.
    async fn get(&self, id: PlanId) -> Result<Option<Plan>>;

    /// Return all `plan_tasks` rows for a plan, sorted ascending by `position`.
    async fn list_plan_tasks_ordered(&self, plan_id: PlanId) -> Result<Vec<PlanTask>>;

    /// Return all plans that contain the given task (for cascade on DeleteTask).
    /// Backed by `idx_plan_tasks_task` so the lookup is O(memberships), not O(tasks).
    async fn list_plans_for_task(&self, task_id: TaskId) -> Result<Vec<PlanId>>;

    /// Apply a persisted event to the projection (mirrors `TaskRepo::apply_event`).
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()>;
}

// ── Run ───────────────────────────────────────────────────────────────────────

/// Read / projection interface for the `runs` table.
#[async_trait]
pub trait RunRepository: Send + Sync {
    /// Fetch a run by id; `None` if not found.
    async fn get(&self, id: RunId) -> Result<Option<Run>>;

    /// Return all runs in `Active` status for the given plan.
    async fn list_active_for_plan(&self, plan_id: PlanId) -> Result<Vec<Run>>;

    /// Return the `task_id` that the run is currently executing
    /// (i.e. the most recent `RunStepStarted` not yet closed by
    /// `RunStepFinished`), or `None` if no step is in-progress.
    async fn current_step_task(&self, run_id: RunId) -> Result<Option<TaskId>>;

    /// §3.7.4 — active runs that have not received a first `RunStepStarted`
    /// within `threshold` after `started_at`, and have not yet emitted
    /// `RunUnresponsive`.
    async fn list_unresponsive_candidates(
        &self,
        threshold: std::time::Duration,
        now: taskagent_shared::Timestamp,
    ) -> Result<Vec<RunId>>;

    /// §3.7.4 — active runs whose `last_activity_at` is at least `threshold`
    /// behind `now`, and have not yet emitted `RunStale`.
    async fn list_stale_candidates(
        &self,
        threshold: std::time::Duration,
        now: taskagent_shared::Timestamp,
    ) -> Result<Vec<RunId>>;

    /// Apply a persisted event to the projection.
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()>;
}

// ── RunNote (§3.8.2) ──────────────────────────────────────────────────────────

/// Read / projection interface for the `run_notes` table.
#[async_trait]
pub trait RunNoteRepository: Send + Sync {
    /// List notes for a run in chronological order. `after` is an opaque
    /// cursor (the id of the last seen note); `limit` is clamped by the impl.
    async fn list_for_run(
        &self,
        run_id: RunId,
        limit: u32,
        after: Option<RunNoteId>,
    ) -> Result<Vec<RunNote>>;

    /// Apply a persisted event to the projection.
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()>;
}

// ── AgentSession ──────────────────────────────────────────────────────────────

/// Read / projection interface for the `agent_sessions` table.
#[async_trait]
pub trait SessionRepository: Send + Sync {
    /// Fetch a session by id; `None` if not found.
    async fn get(&self, id: AgentSessionId) -> Result<Option<AgentSession>>;

    /// Apply a persisted event to the projection.
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()>;
}

// ── AgentClaim ────────────────────────────────────────────────────────────────

/// Read / projection interface for the `agent_claims` table.
#[async_trait]
pub trait AgentClaimRepository: Send + Sync {
    /// Return the agent IDs that currently hold an active (non-expired) claim
    /// on the given task.
    async fn get_agents_claiming_task(&self, task_id: TaskId) -> Result<Vec<AgentId>>;

    /// Apply a persisted event to the projection.
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()>;
}

// ── Work leases ─────────────────────────────────────────────────────────────

/// Projection interface for the `work_leases` table.
#[async_trait]
pub trait WorkLeaseRepository: Send + Sync {
    /// Apply a persisted lease event to the projection (idempotent).
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()>;
}

// ── Document (PR1 §3-4) ───────────────────────────────────────────────────────

/// Read / projection interface for the `documents` table.
#[async_trait]
pub trait DocumentRepository: Send + Sync {
    /// Fetch a document by id; `None` if not found.
    async fn get(&self, id: DocumentId) -> Result<Option<Document>>;

    /// List documents owned by a project.
    ///
    /// - `kind_filter`: when `Some`, returns only documents of that kind.
    /// - `include_archived`: when `false`, soft-archived rows are hidden.
    async fn list_by_project(
        &self,
        project_id: ProjectId,
        kind_filter: Option<DocumentKind>,
        include_archived: bool,
    ) -> Result<Vec<Document>>;

    /// Apply a persisted event to the projection.
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()>;
}

// ── ExternalRef ───────────────────────────────────────────────────────────────

/// Read / projection interface for the `external_refs` table.
#[async_trait]
pub trait ExternalRefRepository: Send + Sync {
    /// Look up an external reference.  Returns the serialised `internal_id`
    /// (e.g. `PlanId::to_string()`) if the mapping exists.
    async fn lookup(&self, tenant: &str, kind: &str, external_id: &str) -> Result<Option<String>>;

    /// Apply a persisted event to the projection.
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()>;
}

// ── Lifecycle rules (docs/LIFECYCLE_RULES_SPEC.md §4) ───────────────────────────

/// Read / projection interface for the `lifecycle_rules` table. Used by the
/// rule-engine gate (effective rules) and CRUD endpoints (get / list).
#[async_trait]
pub trait RuleRepository: Send + Sync {
    /// Fetch a rule by id; `None` if not found.
    async fn get(&self, id: taskagent_shared::RuleId) -> Result<Option<taskagent_domain::Rule>>;

    /// All rules defined directly at a scope level (any enabled state).
    async fn list_for_scope(
        &self,
        scope: &taskagent_domain::RuleScope,
    ) -> Result<Vec<taskagent_domain::Rule>>;

    /// Effective enabled rules for a scope chain firing on `trigger`
    /// (inheritance/override resolved by `rule_key`).
    async fn effective_rules(
        &self,
        chain: &[taskagent_domain::RuleScope],
        trigger: taskagent_domain::RuleTrigger,
    ) -> Result<Vec<taskagent_domain::Rule>>;

    /// Apply a persisted event to the projection.
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()>;
}

// ── Evidence registry (OSS task 019eb65a-3185; spec §1.3) ───────────────────────

/// Read / projection interface for the `evidence` table. Used by the rule-engine
/// gate (to decide whether a `required` rule's requirement is satisfied) and by
/// the evidence CRUD endpoints (get / list).
#[async_trait]
pub trait EvidenceRepository: Send + Sync {
    /// Fetch evidence by id; `None` if not found.
    async fn get(
        &self,
        id: taskagent_shared::EvidenceId,
    ) -> Result<Option<taskagent_domain::Evidence>>;

    /// Evidence recorded directly at a scope level (newest first).
    async fn list_for_scope(
        &self,
        scope: &taskagent_domain::RuleScope,
        include_superseded: bool,
    ) -> Result<Vec<taskagent_domain::Evidence>>;

    /// Gate hot path: does live (non-superseded) evidence of `kind` exist
    /// anywhere in the scope chain, optionally matching `target`?
    async fn has_live_evidence(
        &self,
        chain: &[taskagent_domain::RuleScope],
        kind: taskagent_domain::EvidenceKind,
        target: Option<&str>,
    ) -> Result<bool>;

    /// Apply a persisted event to the projection.
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()>;
}

// ── Concrete implementations ──────────────────────────────────────────────────
//
// `taskagent-core` already depends on `taskagent-storage`, so we implement the
// repository traits here for the concrete storage types.  The `apps/server`
// crate then coerces `Arc<PlanRepo>` → `Arc<dyn PlanRepository>` via the
// builder methods on `CommandHandler`.

use taskagent_events::Event;
use taskagent_storage::{
    AgentClaimRepo, DocumentRepo, EvidenceRepo, ExternalRefRepo, PlanRepo, RuleRepo, RunNoteRepo,
    RunRepo, SessionRepo, WorkLeaseRepo,
};

#[async_trait]
impl EvidenceRepository for EvidenceRepo {
    async fn get(
        &self,
        id: taskagent_shared::EvidenceId,
    ) -> Result<Option<taskagent_domain::Evidence>> {
        EvidenceRepo::get(self, id).await
    }
    async fn list_for_scope(
        &self,
        scope: &taskagent_domain::RuleScope,
        include_superseded: bool,
    ) -> Result<Vec<taskagent_domain::Evidence>> {
        EvidenceRepo::list_for_scope(self, scope, include_superseded).await
    }
    async fn has_live_evidence(
        &self,
        chain: &[taskagent_domain::RuleScope],
        kind: taskagent_domain::EvidenceKind,
        target: Option<&str>,
    ) -> Result<bool> {
        EvidenceRepo::has_live_evidence(self, chain, kind, target).await
    }
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        EvidenceRepo::apply_event(self, env).await
    }
}

#[async_trait]
impl RuleRepository for RuleRepo {
    async fn get(&self, id: taskagent_shared::RuleId) -> Result<Option<taskagent_domain::Rule>> {
        RuleRepo::get(self, id).await
    }
    async fn list_for_scope(
        &self,
        scope: &taskagent_domain::RuleScope,
    ) -> Result<Vec<taskagent_domain::Rule>> {
        RuleRepo::list_for_scope(self, scope).await
    }
    async fn effective_rules(
        &self,
        chain: &[taskagent_domain::RuleScope],
        trigger: taskagent_domain::RuleTrigger,
    ) -> Result<Vec<taskagent_domain::Rule>> {
        RuleRepo::effective_rules(self, chain, trigger).await
    }
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        RuleRepo::apply_event(self, env).await
    }
}

#[async_trait]
impl PlanRepository for PlanRepo {
    async fn get(&self, id: PlanId) -> Result<Option<Plan>> {
        PlanRepo::get(self, id).await
    }
    async fn list_plan_tasks_ordered(&self, plan_id: PlanId) -> Result<Vec<PlanTask>> {
        PlanRepo::list_tasks_ordered(self, plan_id).await
    }
    async fn list_plans_for_task(&self, task_id: TaskId) -> Result<Vec<PlanId>> {
        let plans = PlanRepo::list_plans_for_task(self, task_id).await?;
        Ok(plans.into_iter().map(|p| p.id).collect())
    }
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        PlanRepo::apply_event(self, env).await
    }
}

#[async_trait]
impl RunRepository for RunRepo {
    async fn get(&self, id: RunId) -> Result<Option<Run>> {
        RunRepo::get(self, id).await
    }
    async fn list_active_for_plan(&self, plan_id: PlanId) -> Result<Vec<Run>> {
        RunRepo::list_active_for_plan(self, plan_id).await
    }
    async fn current_step_task(&self, run_id: RunId) -> Result<Option<TaskId>> {
        RunRepo::current_step_task(self, run_id).await
    }
    async fn list_unresponsive_candidates(
        &self,
        threshold: std::time::Duration,
        now: taskagent_shared::Timestamp,
    ) -> Result<Vec<RunId>> {
        RunRepo::list_unresponsive_candidates(self, threshold, now).await
    }
    async fn list_stale_candidates(
        &self,
        threshold: std::time::Duration,
        now: taskagent_shared::Timestamp,
    ) -> Result<Vec<RunId>> {
        RunRepo::list_stale_candidates(self, threshold, now).await
    }
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        RunRepo::apply_event(self, env).await
    }
}

#[async_trait]
impl RunNoteRepository for RunNoteRepo {
    async fn list_for_run(
        &self,
        run_id: RunId,
        limit: u32,
        after: Option<RunNoteId>,
    ) -> Result<Vec<RunNote>> {
        RunNoteRepo::list_for_run(self, run_id, limit, after).await
    }
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        RunNoteRepo::apply_event(self, env).await
    }
}

#[async_trait]
impl SessionRepository for SessionRepo {
    async fn get(&self, id: AgentSessionId) -> Result<Option<AgentSession>> {
        SessionRepo::get(self, id).await
    }
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        SessionRepo::apply_event(self, env).await
    }
}

#[async_trait]
impl AgentClaimRepository for AgentClaimRepo {
    async fn get_agents_claiming_task(&self, task_id: TaskId) -> Result<Vec<AgentId>> {
        AgentClaimRepo::get_agents_claiming_task(self, task_id).await
    }
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        match &env.payload {
            Event::AgentClaimed {
                agent_id,
                task_id,
                expires_at,
            } => self.acquire_until(*agent_id, *task_id, *expires_at).await,
            Event::AgentReleased { agent_id, task_id } => self.release(*agent_id, *task_id).await,
            // Auto-release every claim when the task closes.
            Event::TaskClosed { task_id, .. } => self.release_all_for_task(*task_id).await,
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl WorkLeaseRepository for WorkLeaseRepo {
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        match &env.payload {
            Event::FilesReserved { leases } => {
                for lease in leases {
                    self.apply_reserved(lease).await?;
                }
                Ok(())
            }
            Event::FilesReleased { agent_id, task_id } => {
                self.release_for_task(*agent_id, *task_id).await
            }
            // Auto-release every file lease when the task closes.
            Event::TaskClosed { task_id, .. } => self.release_all_for_task(*task_id).await,
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl DocumentRepository for DocumentRepo {
    async fn get(&self, id: DocumentId) -> Result<Option<Document>> {
        DocumentRepo::get(self, id).await
    }
    async fn list_by_project(
        &self,
        project_id: ProjectId,
        kind_filter: Option<DocumentKind>,
        include_archived: bool,
    ) -> Result<Vec<Document>> {
        DocumentRepo::list_by_project(self, project_id, kind_filter, include_archived).await
    }
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        DocumentRepo::apply_event(self, env).await
    }
}

#[async_trait]
impl ExternalRefRepository for ExternalRefRepo {
    async fn lookup(&self, tenant: &str, kind: &str, external_id: &str) -> Result<Option<String>> {
        ExternalRefRepo::lookup(self, tenant, kind, external_id).await
    }
    async fn apply_event(&self, _env: &EventEnvelope) -> Result<()> {
        // No events currently update the external_refs projection.
        Ok(())
    }
}
