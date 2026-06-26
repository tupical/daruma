//! Plan concurrency helpers — NextTaskResolver and parent-cycle detection.
//!
//! `NextTaskResolver` picks the next eligible task for an agent to work on
//! inside a Plan.  `detect_parent_cycle` guards the `parent_plan_id` chain
//! against cycles and excessive nesting depth.

use std::time::Duration;

use daruma_domain::{PlanStatus, Status};
use daruma_shared::{time, AgentId, CoreError, PlanId, Result, RunId, TaskId, Timestamp};
use daruma_storage::{AgentClaimRepo, RelationRepo, TaskRepo};

use crate::relation_enforcement::list_active_blockers;
use crate::repos::PlanRepository;

/// Maximum allowed nesting depth for `parent_plan_id` chains (§3.1 §9).
pub const MAX_PARENT_DEPTH: u32 = 3;

// ── NextTask ──────────────────────────────────────────────────────────────────

/// The result of `NextTaskResolver::next` — the first eligible task in a plan.
pub struct NextTask {
    pub task_id: TaskId,
    pub position: u32,
    /// If a TTL was requested the caller should dispatch `Command::AcquireClaim`
    /// with this `expires_at`.  The resolver does **not** emit commands itself.
    pub claim_expires_at: Option<Timestamp>,
}

// ── NextTaskResolver ──────────────────────────────────────────────────────────

/// Stateless resolver: given a plan and optional claim TTL, returns the first
/// task an agent should execute.
///
/// Algorithm:
/// 1. Load plan → status must be `Active`, else return `None`.
/// 2. List `plan_tasks` ordered by `position`.
/// 3. Skip tasks whose `status == Done`.
/// 4. Skip tasks whose `depends_on` contains any non-Done task.
/// 5. Skip tasks with an active cross-task `Blocks` blocker (same
///    semantics as `can_start`) — without this, two agents can grab
///    tasks that block each other.
/// 6. Skip tasks already claimed by a *different* agent (claim-aware).
/// 7. Return the first survivor + (optionally) compute `claim_expires_at`.
pub struct NextTaskResolver<'a> {
    pub plans: &'a dyn PlanRepository,
    pub tasks: &'a TaskRepo,
    pub claims: &'a AgentClaimRepo,
    /// Cross-task `Blocks` relations. `None` only in legacy/unit contexts;
    /// production call sites must pass the relation repo so the resolver
    /// honors blockers that live outside `plan_tasks.depends_on`.
    pub relations: Option<&'a RelationRepo>,
}

impl NextTaskResolver<'_> {
    pub async fn next(
        &self,
        plan_id: PlanId,
        _run_id: RunId,
        agent_id: AgentId,
        claim_ttl: Option<Duration>,
    ) -> Result<Option<NextTask>> {
        // 1. Plan must be Active
        let plan = self
            .plans
            .get(plan_id)
            .await?
            .ok_or_else(|| CoreError::not_found(format!("plan {plan_id}")))?;

        if plan.status != PlanStatus::Active {
            return Ok(None);
        }

        // 2. Ordered task list
        let plan_tasks = self.plans.list_plan_tasks_ordered(plan_id).await?;

        // 3+4. Filter
        for pt in &plan_tasks {
            // Task must exist
            let task = match self.tasks.get(pt.task_id).await? {
                Some(t) => t,
                None => continue,
            };

            // Skip done tasks
            if task.status == Status::Done {
                continue;
            }

            // All dependencies must be Done
            let mut deps_ok = true;
            for dep_id in &pt.depends_on {
                match self.tasks.get(*dep_id).await? {
                    Some(dep) if dep.status == Status::Done => {}
                    _ => {
                        deps_ok = false;
                        break;
                    }
                }
            }
            if !deps_ok {
                continue;
            }

            // 5. Cross-task Blocks relations must be satisfied too (matches
            //    can_start): a candidate with a live blocker is not ready,
            //    even when its plan-level depends_on list is clear.
            if let Some(relations) = self.relations {
                if !list_active_blockers(relations, self.tasks, pt.task_id)
                    .await?
                    .is_empty()
                {
                    continue;
                }
            }

            // 6. Skip tasks already claimed by a different agent so concurrent
            //    resolvers don't all return the same task.
            if self
                .claims
                .is_claimed_by_other(pt.task_id, agent_id)
                .await?
                .is_some()
            {
                continue;
            }

            // 7. Found candidate — compute optional claim expiry
            let claim_expires_at = claim_ttl.map(|ttl| {
                let secs = ttl.as_secs().min(i64::MAX as u64) as i64;
                time::now() + chrono::Duration::seconds(secs)
            });

            return Ok(Some(NextTask {
                task_id: pt.task_id,
                position: pt.position,
                claim_expires_at,
            }));
        }

        Ok(None)
    }
}

// ── detect_parent_cycle ───────────────────────────────────────────────────────

/// Check that setting `candidate_parent` as the `parent_plan_id` of `plan_id`
/// would not create a cycle or exceed [`MAX_PARENT_DEPTH`].
///
/// Returns `Err(CoreError::Validation("cycle_detected"))` when:
/// - `candidate_parent == plan_id` (self-reference), or
/// - Walking upward from `candidate_parent` encounters `plan_id`, or
/// - The upward chain exceeds `MAX_PARENT_DEPTH` hops.
pub async fn detect_parent_cycle(
    plans: &dyn PlanRepository,
    plan_id: PlanId,
    candidate_parent: PlanId,
) -> Result<()> {
    // Immediate self-reference
    if candidate_parent == plan_id {
        return Err(CoreError::validation("cycle_detected"));
    }

    let mut current = candidate_parent;
    let mut hops = 0u32;

    loop {
        hops += 1;
        if hops > MAX_PARENT_DEPTH {
            return Err(CoreError::validation("cycle_detected"));
        }

        let plan = match plans.get(current).await? {
            Some(p) => p,
            None => break, // reached a non-existent ancestor → no cycle, within depth
        };

        match plan.parent_plan_id {
            None => break, // reached root → no cycle
            Some(parent) => {
                if parent == plan_id {
                    return Err(CoreError::validation("cycle_detected"));
                }
                current = parent;
            }
        }
    }

    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repos::PlanRepository;
    use async_trait::async_trait;
    use std::{collections::HashMap, sync::Mutex};
    use daruma_domain::{Plan, PlanStatus as PS, PlanTask};
    use daruma_events::EventEnvelope;
    use daruma_shared::{time, ProjectId};

    // ── Minimal stub ──────────────────────────────────────────────────────────

    #[derive(Default)]
    struct MemPlanRepo {
        plans: Mutex<HashMap<PlanId, Plan>>,
        tasks: Mutex<HashMap<PlanId, Vec<PlanTask>>>,
    }

    impl MemPlanRepo {
        fn insert(&self, plan: Plan) {
            self.plans.lock().unwrap().insert(plan.id, plan);
        }
    }

    #[async_trait]
    impl PlanRepository for MemPlanRepo {
        async fn get(&self, id: PlanId) -> Result<Option<Plan>> {
            Ok(self.plans.lock().unwrap().get(&id).cloned())
        }

        async fn list_plan_tasks_ordered(&self, plan_id: PlanId) -> Result<Vec<PlanTask>> {
            let mut v = self
                .tasks
                .lock()
                .unwrap()
                .get(&plan_id)
                .cloned()
                .unwrap_or_default();
            v.sort_by_key(|t| t.position);
            Ok(v)
        }

        async fn list_plans_for_task(&self, _task_id: TaskId) -> Result<Vec<PlanId>> {
            Ok(vec![])
        }

        async fn apply_event(&self, _env: &EventEnvelope) -> Result<()> {
            Ok(())
        }
    }

    fn plan(id: PlanId, parent: Option<PlanId>, status: PS) -> Plan {
        let now = time::now();
        Plan {
            id,
            project_id: ProjectId::new(),
            parent_plan_id: parent,
            title: "t".into(),
            description: String::new(),
            goal: String::new(),
            success_criteria: vec![],
            status,
            owner: daruma_domain::Actor::user(),
            created_at: now,
            updated_at: now,
            archived_at: None,
            source_brief: None,
        }
    }

    // ── detect_parent_cycle ───────────────────────────────────────────────────

    #[tokio::test]
    async fn no_cycle_flat_parent() {
        let repo = MemPlanRepo::default();
        let parent_id = PlanId::new();
        repo.insert(plan(parent_id, None, PS::Active));
        let new_id = PlanId::new();
        assert!(detect_parent_cycle(&repo, new_id, parent_id).await.is_ok());
    }

    #[tokio::test]
    async fn self_reference_is_cycle() {
        let repo = MemPlanRepo::default();
        let id = PlanId::new();
        let err = detect_parent_cycle(&repo, id, id).await.unwrap_err();
        assert!(matches!(err, CoreError::Validation(_)));
    }

    #[tokio::test]
    async fn depth_at_max_is_ok() {
        // A → B → C  (MAX_PARENT_DEPTH = 3, new plan D → C is depth 3 = ok)
        let repo = MemPlanRepo::default();
        let a = PlanId::new();
        let b = PlanId::new();
        let c = PlanId::new();
        repo.insert(plan(a, None, PS::Active));
        repo.insert(plan(b, Some(a), PS::Active));
        repo.insert(plan(c, Some(b), PS::Active));
        let d = PlanId::new();
        assert!(detect_parent_cycle(&repo, d, c).await.is_ok());
    }

    #[tokio::test]
    async fn depth_exceeding_max_is_error() {
        // A → B → C → D, new plan E → D would exceed depth
        let repo = MemPlanRepo::default();
        let a = PlanId::new();
        let b = PlanId::new();
        let c = PlanId::new();
        let d = PlanId::new();
        repo.insert(plan(a, None, PS::Active));
        repo.insert(plan(b, Some(a), PS::Active));
        repo.insert(plan(c, Some(b), PS::Active));
        repo.insert(plan(d, Some(c), PS::Active));
        let e = PlanId::new();
        let err = detect_parent_cycle(&repo, e, d).await.unwrap_err();
        assert!(matches!(err, CoreError::Validation(_)));
    }

    #[tokio::test]
    async fn traversal_cycle_detected() {
        // A (parent=B), B (parent=C), try: set C.parent = A
        let repo = MemPlanRepo::default();
        let a = PlanId::new();
        let b = PlanId::new();
        let c = PlanId::new();
        repo.insert(plan(a, Some(b), PS::Active));
        repo.insert(plan(b, Some(c), PS::Active));
        // c currently has no parent — setting c.parent = a would complete a cycle
        // detect_parent_cycle(repo, c, a): from a → parent=b → parent=c == plan_id → error
        let err = detect_parent_cycle(&repo, c, a).await.unwrap_err();
        assert!(matches!(err, CoreError::Validation(_)));
    }
}
