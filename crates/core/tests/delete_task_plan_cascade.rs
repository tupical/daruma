//! Integration tests for DeleteTask → PlanTaskRemoved cascade (§3.2.6 / W0.2).
//!
//! Covers:
//! - PlanTaskRemoved is emitted (before TaskDeleted) when a task belongs to a plan.
//! - The plan projection (plan_tasks) shrinks by 1 after deletion.
//! - plan_next_task (via list_plan_tasks_ordered) no longer returns the deleted task.

use std::sync::Arc;

use daruma_core::{Command, CommandHandler};
use daruma_domain::{Actor, NewPlan, NewTask, RelationKind};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::{PlanId, ProjectId, TaskId};
use daruma_storage::{
    ActivityRepo, CommentRepo, Db, PlanRepo, ProjectRepo, RelationRepo, SqliteEventStore, TaskRepo,
};

use daruma_core::repos::PlanRepository;

// ── Test stack ────────────────────────────────────────────────────────────────

/// Build a handler wired with real SQLite repos (in-memory) including PlanRepo.
async fn build_stack() -> (CommandHandler, Arc<PlanRepo>, Arc<TaskRepo>) {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();

    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let tasks = Arc::new(TaskRepo::new(pool.clone()));
    let projects = Arc::new(ProjectRepo::new(pool.clone()));
    let comments = Arc::new(CommentRepo::new(pool.clone()));
    let activity = Arc::new(ActivityRepo::new(pool.clone()));
    let relations = Arc::new(RelationRepo::new(pool.clone()));
    let plans = Arc::new(PlanRepo::new(pool.clone()));
    let bus = EventBus::default();

    let handler = CommandHandler::new(store, tasks.clone(), projects, comments, activity, bus)
        .with_relations(relations)
        .with_plans(plans.clone() as Arc<dyn PlanRepository>);

    (handler, plans, tasks)
}

/// Create a task and return its id.
async fn create_task(handler: &CommandHandler, title: &str) -> TaskId {
    let envs = handler
        .handle(
            Command::CreateTask {
                task: NewTask::new(title),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    match &envs[0].payload {
        Event::TaskCreated { task } => task.id.unwrap(),
        _ => panic!("expected TaskCreated"),
    }
}

/// Create a plan and return its id (stays in Draft status — sufficient for task attachment).
async fn create_plan(handler: &CommandHandler, project_id: ProjectId) -> PlanId {
    let envs = handler
        .handle(
            Command::CreatePlan {
                plan: NewPlan::new("Cascade test plan", project_id, Actor::user()),
                external_ref: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    match &envs[0].payload {
        Event::PlanCreated { plan } => plan.id,
        _ => panic!("expected PlanCreated"),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Scenario 1: DeleteTask emits PlanTaskRemoved BEFORE TaskDeleted.
#[tokio::test]
async fn delete_task_emits_plan_task_removed_before_task_deleted() {
    let (handler, _plans, _tasks) = build_stack().await;
    let project_id = ProjectId::new();

    let task_id = create_task(&handler, "Plan member task").await;
    let plan_id = create_plan(&handler, project_id).await;

    // Attach task to plan.
    handler
        .handle(
            Command::AddPlanTask {
                plan_id,
                task_id,
                position: Some(0),
                depends_on: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // Delete the task.
    let envs = handler
        .handle(Command::DeleteTask { id: task_id }, Actor::user())
        .await
        .unwrap();

    // Find PlanTaskRemoved and TaskDeleted positions.
    let removed_pos = envs.iter().position(|e| {
        matches!(
            &e.payload,
            Event::PlanTaskRemoved {
                plan_id: p,
                task_id: t,
            } if *p == plan_id && *t == task_id
        )
    });
    let deleted_pos = envs
        .iter()
        .position(|e| matches!(&e.payload, Event::TaskDeleted { task_id: t } if *t == task_id));

    assert!(
        removed_pos.is_some(),
        "expected PlanTaskRemoved in events, got: {:?}",
        envs.iter().map(|e| &e.payload).collect::<Vec<_>>()
    );
    assert!(
        deleted_pos.is_some(),
        "expected TaskDeleted in events, got: {:?}",
        envs.iter().map(|e| &e.payload).collect::<Vec<_>>()
    );
    assert!(
        removed_pos.unwrap() < deleted_pos.unwrap(),
        "PlanTaskRemoved must appear before TaskDeleted"
    );
}

/// Scenario 2: After deletion, plan_tasks projection shrinks and plan_next_task
/// no longer returns the deleted task.
#[tokio::test]
async fn delete_task_cleans_plan_projection() {
    let (handler, plans, _tasks) = build_stack().await;
    let project_id = ProjectId::new();

    let task_a = create_task(&handler, "Task A").await;
    let task_b = create_task(&handler, "Task B (to delete)").await;
    let plan_id = create_plan(&handler, project_id).await;

    // Attach both tasks.
    handler
        .handle(
            Command::AddPlanTask {
                plan_id,
                task_id: task_a,
                position: Some(0),
                depends_on: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    handler
        .handle(
            Command::AddPlanTask {
                plan_id,
                task_id: task_b,
                position: Some(1),
                depends_on: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // Before deletion: 2 tasks in plan.
    let before = plans.list_tasks_ordered(plan_id).await.unwrap();
    assert_eq!(before.len(), 2, "expected 2 plan tasks before deletion");

    // Delete task_b.
    handler
        .handle(Command::DeleteTask { id: task_b }, Actor::user())
        .await
        .unwrap();

    // After deletion: only task_a remains.
    let after = plans.list_tasks_ordered(plan_id).await.unwrap();
    assert_eq!(
        after.len(),
        1,
        "plan tasks_total should shrink to 1 after deletion"
    );
    assert_eq!(
        after[0].task_id, task_a,
        "only task_a should remain in plan"
    );

    // plan_next_task equivalent: the first entry must not be the deleted task.
    assert_ne!(
        after[0].task_id, task_b,
        "deleted task must not appear in plan_next_task results"
    );
}

/// Scenario 3+: DeleteTask with both a relation AND a plan membership emits
/// TaskUnlinked AND PlanTaskRemoved, both before TaskDeleted.
#[tokio::test]
async fn delete_task_emits_task_unlinked_and_plan_task_removed() {
    let (handler, _plans, _tasks) = build_stack().await;
    let project_id = ProjectId::new();

    let task_to_delete = create_task(&handler, "Task to delete").await;
    let blocker_task = create_task(&handler, "Blocker").await;
    let plan_id = create_plan(&handler, project_id).await;

    // Attach task_to_delete to plan.
    handler
        .handle(
            Command::AddPlanTask {
                plan_id,
                task_id: task_to_delete,
                position: Some(0),
                depends_on: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // blocker_task blocks task_to_delete.
    handler
        .handle(
            Command::LinkTasks {
                from: blocker_task,
                to: task_to_delete,
                kind: RelationKind::Blocks,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // Delete — should cascade both TaskUnlinked and PlanTaskRemoved.
    let envs = handler
        .handle(Command::DeleteTask { id: task_to_delete }, Actor::user())
        .await
        .unwrap();

    let deleted_pos = envs
        .iter()
        .position(
            |e| matches!(&e.payload, Event::TaskDeleted { task_id: t } if *t == task_to_delete),
        )
        .expect("TaskDeleted must be emitted");

    let unlinked_pos = envs
        .iter()
        .position(|e| matches!(&e.payload, Event::TaskUnlinked { .. }))
        .expect("TaskUnlinked must be emitted for the relation");

    let removed_pos = envs
        .iter()
        .position(|e| {
            matches!(
                &e.payload,
                Event::PlanTaskRemoved {
                    plan_id: p,
                    task_id: t,
                } if *p == plan_id && *t == task_to_delete
            )
        })
        .expect("PlanTaskRemoved must be emitted for the plan");

    assert!(
        unlinked_pos < deleted_pos,
        "TaskUnlinked must appear before TaskDeleted"
    );
    assert!(
        removed_pos < deleted_pos,
        "PlanTaskRemoved must appear before TaskDeleted"
    );
}

/// Scenario 3: Deleting a task not in any plan emits no PlanTaskRemoved.
#[tokio::test]
async fn delete_task_not_in_plan_emits_no_plan_task_removed() {
    let (handler, _plans, _tasks) = build_stack().await;

    let task_id = create_task(&handler, "Standalone task").await;

    let envs = handler
        .handle(Command::DeleteTask { id: task_id }, Actor::user())
        .await
        .unwrap();

    let any_removed = envs
        .iter()
        .any(|e| matches!(&e.payload, Event::PlanTaskRemoved { .. }));
    assert!(
        !any_removed,
        "no PlanTaskRemoved expected for task not in any plan"
    );

    let deleted = envs
        .iter()
        .any(|e| matches!(&e.payload, Event::TaskDeleted { task_id: t } if *t == task_id));
    assert!(deleted, "TaskDeleted must still be emitted");
}
