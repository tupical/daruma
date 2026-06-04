//! Integration tests for `detect_parent_cycle` (plan §2 AC-8).

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use taskagent_core::{detect_parent_cycle, MAX_PARENT_DEPTH};
use taskagent_domain::{Plan, PlanStatus, PlanTask};
use taskagent_events::EventEnvelope;
use taskagent_shared::{time, CoreError, PlanId, ProjectId, Result, TaskId};

// ── In-memory stub ────────────────────────────────────────────────────────────

#[derive(Default)]
struct MemPlanRepo {
    plans: Mutex<HashMap<PlanId, Plan>>,
}

impl MemPlanRepo {
    fn insert(&self, plan: Plan) {
        self.plans.lock().unwrap().insert(plan.id, plan);
    }
}

#[async_trait]
impl taskagent_core::repos::PlanRepository for MemPlanRepo {
    async fn get(&self, id: PlanId) -> Result<Option<Plan>> {
        Ok(self.plans.lock().unwrap().get(&id).cloned())
    }

    async fn list_plan_tasks_ordered(&self, _plan_id: PlanId) -> Result<Vec<PlanTask>> {
        Ok(vec![])
    }

    async fn list_plans_for_task(&self, _task_id: TaskId) -> Result<Vec<PlanId>> {
        Ok(vec![])
    }

    async fn apply_event(&self, _env: &EventEnvelope) -> Result<()> {
        Ok(())
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn plan(id: PlanId, parent: Option<PlanId>) -> Plan {
    let now = time::now();
    Plan {
        id,
        project_id: ProjectId::new(),
        parent_plan_id: parent,
        title: "t".into(),
        description: String::new(),
        goal: String::new(),
        success_criteria: vec![],
        status: PlanStatus::Active,
        owner: taskagent_domain::Actor::user(),
        created_at: now,
        updated_at: now,
        archived_at: None,
        source_brief: None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn no_cycle_root_parent() {
    let repo = MemPlanRepo::default();
    let parent_id = PlanId::new();
    repo.insert(plan(parent_id, None));
    let new_id = PlanId::new();
    assert!(detect_parent_cycle(&repo, new_id, parent_id).await.is_ok());
}

#[tokio::test]
async fn self_reference_rejected() {
    let repo = MemPlanRepo::default();
    let id = PlanId::new();
    let err = detect_parent_cycle(&repo, id, id).await.unwrap_err();
    assert!(
        matches!(err, CoreError::Validation(_)),
        "expected Validation error, got {err:?}"
    );
}

#[tokio::test]
async fn two_plan_cycle_rejected() {
    // A.parent = B, try B.parent = A → cycle
    let repo = MemPlanRepo::default();
    let a = PlanId::new();
    let b = PlanId::new();
    repo.insert(plan(a, Some(b)));
    repo.insert(plan(b, None));
    let err = detect_parent_cycle(&repo, b, a).await.unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)));
}

#[tokio::test]
async fn three_plan_cycle_rejected() {
    // A → B → C, try C.parent = A
    let repo = MemPlanRepo::default();
    let a = PlanId::new();
    let b = PlanId::new();
    let c = PlanId::new();
    repo.insert(plan(a, Some(b)));
    repo.insert(plan(b, Some(c)));
    repo.insert(plan(c, None));
    let err = detect_parent_cycle(&repo, c, a).await.unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)));
}

#[tokio::test]
async fn depth_at_max_is_accepted() {
    // chain A → B → C (depth 2), new D → C = depth 3 = MAX_PARENT_DEPTH → ok
    let repo = MemPlanRepo::default();
    let a = PlanId::new();
    let b = PlanId::new();
    let c = PlanId::new();
    repo.insert(plan(a, None));
    repo.insert(plan(b, Some(a)));
    repo.insert(plan(c, Some(b)));
    let d = PlanId::new();
    assert!(
        detect_parent_cycle(&repo, d, c).await.is_ok(),
        "depth == MAX_PARENT_DEPTH should be allowed"
    );
}

#[tokio::test]
async fn depth_exceeding_max_rejected() {
    // A → B → C → D, new E → D would exceed MAX_PARENT_DEPTH
    let repo = MemPlanRepo::default();
    let a = PlanId::new();
    let b = PlanId::new();
    let c = PlanId::new();
    let d = PlanId::new();
    repo.insert(plan(a, None));
    repo.insert(plan(b, Some(a)));
    repo.insert(plan(c, Some(b)));
    repo.insert(plan(d, Some(c)));
    let e = PlanId::new();
    let err = detect_parent_cycle(&repo, e, d).await.unwrap_err();
    assert!(
        matches!(err, CoreError::Validation(_)),
        "depth > MAX_PARENT_DEPTH ({MAX_PARENT_DEPTH}) must be rejected"
    );
}

#[tokio::test]
async fn max_parent_depth_constant_is_three() {
    assert_eq!(MAX_PARENT_DEPTH, 3);
}
