//! Repository traits for plan-domain aggregates.
//!
//! These traits are the contract between `daruma-core` (commands,
//! concurrency logic) and `daruma-storage` (concrete SQLite
//! implementations).  A trait lives here only where tests supply an
//! in-memory stub impl; a repository with a single implementation is
//! held as its concrete `daruma-storage` type instead.

use async_trait::async_trait;
use daruma_domain::{AgentSession, Plan, PlanTask, Run};
use daruma_events::EventEnvelope;
use daruma_shared::{AgentSessionId, PlanId, Result, RunId, TaskId};

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
        now: daruma_shared::Timestamp,
    ) -> Result<Vec<RunId>>;

    /// §3.7.4 — active runs whose `last_activity_at` is at least `threshold`
    /// behind `now`, and have not yet emitted `RunStale`.
    async fn list_stale_candidates(
        &self,
        threshold: std::time::Duration,
        now: daruma_shared::Timestamp,
    ) -> Result<Vec<RunId>>;

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

// ── Concrete implementations ──────────────────────────────────────────────────
//
// `daruma-core` already depends on `daruma-storage`, so we implement the
// repository traits here for the concrete storage types.  The `apps/server`
// crate then coerces `Arc<PlanRepo>` → `Arc<dyn PlanRepository>` via the
// builder methods on `CommandHandler`.

use daruma_storage::{ExternalRefRepo, PlanRepo, RunRepo, SessionRepo};

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
        now: daruma_shared::Timestamp,
    ) -> Result<Vec<RunId>> {
        RunRepo::list_unresponsive_candidates(self, threshold, now).await
    }
    async fn list_stale_candidates(
        &self,
        threshold: std::time::Duration,
        now: daruma_shared::Timestamp,
    ) -> Result<Vec<RunId>> {
        RunRepo::list_stale_candidates(self, threshold, now).await
    }
    async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        RunRepo::apply_event(self, env).await
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
impl ExternalRefRepository for ExternalRefRepo {
    async fn lookup(&self, tenant: &str, kind: &str, external_id: &str) -> Result<Option<String>> {
        ExternalRefRepo::lookup(self, tenant, kind, external_id).await
    }
    async fn apply_event(&self, _env: &EventEnvelope) -> Result<()> {
        // No events currently update the external_refs projection.
        Ok(())
    }
}
