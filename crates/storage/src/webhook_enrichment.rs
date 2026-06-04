//! §3.7.5 — storage-backed implementation of [`EnrichmentSource`].
//!
//! Resolves the three keys the dispatcher knows about:
//!
//! | key             | resolves for                                   | shape                                                   |
//! |-----------------|-----------------------------------------------|---------------------------------------------------------|
//! | `parent_plan`   | any event with `target_task()`                | `{ id, title, status }` of the first plan that owns it  |
//! | `project`       | any event with `target_task()`/`target_plan`  | `{ id, title }` of the owning project                   |
//! | `task`          | any event with `target_task()`                | `{ id, title, status, priority }`                       |
//!
//! Unknown keys return `None` so the dispatcher silently skips them.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use taskagent_events::{Event, EventEnvelope};
use taskagent_shared::{PlanId, ProjectId, TaskId};
use taskagent_webhooks::enrich::{keys, EnrichmentSource};

use crate::{PlanRepo, ProjectRepo, TaskRepo};

/// Composable enrichment source backed by the projection repos.
#[derive(Clone)]
pub struct WebhookEnrichment {
    tasks: Arc<TaskRepo>,
    plans: Arc<PlanRepo>,
    projects: Arc<ProjectRepo>,
}

impl WebhookEnrichment {
    pub fn new(tasks: Arc<TaskRepo>, plans: Arc<PlanRepo>, projects: Arc<ProjectRepo>) -> Self {
        Self {
            tasks,
            plans,
            projects,
        }
    }

    /// Wrap in an `Arc<dyn EnrichmentSource>` for the dispatcher.
    pub fn into_arc(self) -> Arc<dyn EnrichmentSource> {
        Arc::new(self)
    }

    async fn resolve_parent_plan(&self, task_id: TaskId) -> Option<Value> {
        let plans = self.plans.list_plans_for_task(task_id).await.ok()?;
        let plan = plans.into_iter().next()?;
        Some(json!({
            "id": plan.id.to_string(),
            "title": plan.title,
            "status": plan.status,
        }))
    }

    async fn resolve_project_by_id(&self, project_id: ProjectId) -> Option<Value> {
        let project = self.projects.get(project_id).await.ok()??;
        Some(json!({
            "id": project.id.to_string(),
            "title": project.title,
        }))
    }

    async fn resolve_project_via_task(&self, task_id: TaskId) -> Option<Value> {
        let task = self.tasks.get(task_id).await.ok()??;
        let project_id = task.project_id?;
        self.resolve_project_by_id(project_id).await
    }

    async fn resolve_task(&self, task_id: TaskId) -> Option<Value> {
        let task = self.tasks.get(task_id).await.ok()??;
        Some(json!({
            "id": task.id.to_string(),
            "title": task.title,
            "status": task.status,
            "priority": task.priority,
        }))
    }
}

#[async_trait]
impl EnrichmentSource for WebhookEnrichment {
    async fn resolve(&self, key: &str, env: &EventEnvelope) -> Option<Value> {
        match key {
            keys::PARENT_PLAN => {
                let task_id = env.payload.target_task()?;
                self.resolve_parent_plan(task_id).await
            }
            keys::PROJECT => {
                if let Some(pid) = env.payload.target_project() {
                    return self.resolve_project_by_id(pid).await;
                }
                if let Some(plan_id) = plan_id_of(&env.payload) {
                    let plan = self.plans.get(plan_id).await.ok()??;
                    return self.resolve_project_by_id(plan.project_id).await;
                }
                let task_id = env.payload.target_task()?;
                self.resolve_project_via_task(task_id).await
            }
            keys::TASK => {
                let task_id = env.payload.target_task()?;
                self.resolve_task(task_id).await
            }
            other => {
                tracing::warn!(key = %other, "unknown webhook enrich key");
                None
            }
        }
    }
}

/// Extract the plan id from variants that carry one inline. Useful for
/// resolving `project` on plan events that don't expose `target_project()`.
fn plan_id_of(ev: &Event) -> Option<PlanId> {
    match ev {
        Event::PlanCreated { plan } => Some(plan.id),
        Event::PlanUpdated { plan_id, .. }
        | Event::PlanStatusChanged { plan_id, .. }
        | Event::PlanGoalChanged { plan_id, .. }
        | Event::PlanTaskAdded { plan_id, .. }
        | Event::PlanTaskRemoved { plan_id, .. }
        | Event::PlanReordered { plan_id, .. }
        | Event::PlanArchived { plan_id, .. }
        | Event::PlanModifiedByHuman { plan_id, .. }
        | Event::RunObsolescedByPlanEdit { plan_id, .. } => Some(*plan_id),
        _ => None,
    }
}
