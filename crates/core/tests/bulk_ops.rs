//! Integration tests for bulk-op commands (§3.7.7 / LIN B.7).
//!
//! Covers `Command::BulkSetStatus` and `Command::BulkAttachToPlan`:
//! cap enforcement, deduplication, fail-fast on missing ids, idempotent
//! attach, and event count parity with the single-id paths.

use std::sync::Arc;

use taskagent_core::{repos::PlanRepository, Command, CommandHandler};
use taskagent_domain::{Actor, NewPlan, NewTask, Status};
use taskagent_events::{Event, EventBus, EventStore};
use taskagent_shared::{CoreError, PlanId, ProjectId, TaskId};
use taskagent_storage::{
    ActivityRepo, CommentRepo, Db, PlanRepo, ProjectRepo, RelationRepo, SqliteEventStore, TaskRepo,
};

// ── Stack builder ─────────────────────────────────────────────────────────────

async fn build_stack() -> (CommandHandler, Arc<TaskRepo>, Arc<PlanRepo>) {
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

    (handler, tasks, plans)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
        other => panic!("expected TaskCreated, got: {other:?}"),
    }
}

async fn create_plan(handler: &CommandHandler, project_id: ProjectId) -> PlanId {
    let envs = handler
        .handle(
            Command::CreatePlan {
                plan: NewPlan::new("Bulk plan", project_id, Actor::user()),
                external_ref: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    match &envs[0].payload {
        Event::PlanCreated { plan } => plan.id,
        other => panic!("expected PlanCreated, got: {other:?}"),
    }
}

// ── BulkSetStatus tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn bulk_set_status_changes_all_provided_tasks() {
    let (handler, tasks_repo, _plans) = build_stack().await;
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(create_task(&handler, &format!("t{i}")).await);
    }

    handler
        .handle(
            Command::BulkSetStatus {
                ids: ids.clone(),
                status: Status::InProgress,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    for id in &ids {
        let task = tasks_repo.get(*id).await.unwrap().unwrap();
        assert_eq!(task.status, Status::InProgress, "task {id} not updated");
    }
}

#[tokio::test]
async fn bulk_set_status_dedupes_ids() {
    let (handler, _tasks_repo, _plans) = build_stack().await;
    let a = create_task(&handler, "a").await;
    let b = create_task(&handler, "b").await;

    // a appears 3x, b 2x — must dedupe to 2 status transitions.
    let envs = handler
        .handle(
            Command::BulkSetStatus {
                ids: vec![a, b, a, a, b],
                status: Status::Todo,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    let status_changes = envs
        .iter()
        .filter(|e| matches!(e.payload, Event::TaskStatusChanged { .. }))
        .count();
    assert_eq!(
        status_changes, 2,
        "expected exactly 2 TaskStatusChanged events"
    );
}

#[tokio::test]
async fn bulk_set_status_rejects_oversized_request() {
    let (handler, _tasks_repo, _plans) = build_stack().await;

    // 51 random ids — none exist, but the cap check fires first.
    let ids: Vec<TaskId> = (0..51).map(|_| TaskId::new()).collect();

    let err = handler
        .handle(
            Command::BulkSetStatus {
                ids,
                status: Status::Todo,
            },
            Actor::user(),
        )
        .await
        .unwrap_err();

    assert!(
        matches!(err, CoreError::Validation(_)),
        "expected Validation, got: {err:?}"
    );
    let msg = format!("{err}");
    assert!(msg.contains("bulk size"), "msg missing cap reason: {msg}");
    assert!(msg.contains("50"), "msg missing cap value: {msg}");
}

#[tokio::test]
async fn bulk_set_status_rejects_empty_request() {
    let (handler, _tasks_repo, _plans) = build_stack().await;
    let err = handler
        .handle(
            Command::BulkSetStatus {
                ids: Vec::new(),
                status: Status::Todo,
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)));
}

#[tokio::test]
async fn bulk_set_status_fails_fast_on_missing_id() {
    let (handler, tasks_repo, _plans) = build_stack().await;
    let a = create_task(&handler, "a").await;
    let b = create_task(&handler, "b").await;
    let phantom = TaskId::new();

    let err = handler
        .handle(
            Command::BulkSetStatus {
                ids: vec![a, phantom, b],
                status: Status::InProgress,
            },
            Actor::user(),
        )
        .await
        .unwrap_err();

    assert!(
        matches!(err, CoreError::NotFound(_)),
        "expected NotFound, got: {err:?}"
    );

    // Atomicity: no task should have transitioned.
    for id in [a, b] {
        let task = tasks_repo.get(id).await.unwrap().unwrap();
        assert_eq!(
            task.status,
            Status::Inbox,
            "task {id} must not transition when batch fails"
        );
    }
}

#[tokio::test]
async fn bulk_set_status_emits_one_event_per_task() {
    let (handler, _tasks_repo, _plans) = build_stack().await;
    let mut ids = Vec::new();
    for i in 0..3 {
        ids.push(create_task(&handler, &format!("t{i}")).await);
    }

    let envs = handler
        .handle(
            Command::BulkSetStatus {
                ids,
                status: Status::Todo,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    let status_changes = envs
        .iter()
        .filter(|e| matches!(e.payload, Event::TaskStatusChanged { .. }))
        .count();
    assert_eq!(
        status_changes, 3,
        "expected 3 TaskStatusChanged (one per task)"
    );
}

#[tokio::test]
async fn bulk_set_status_skips_already_in_target_status() {
    let (handler, _tasks_repo, _plans) = build_stack().await;
    let a = create_task(&handler, "a").await;
    let b = create_task(&handler, "b").await;

    // First move `a` to Todo so the bulk call below becomes a partial no-op.
    handler
        .handle(
            Command::SetStatus {
                id: a,
                status: Status::Todo,
                force: false,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    let envs = handler
        .handle(
            Command::BulkSetStatus {
                ids: vec![a, b],
                status: Status::Todo,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    let status_changes = envs
        .iter()
        .filter(|e| matches!(e.payload, Event::TaskStatusChanged { .. }))
        .count();
    assert_eq!(status_changes, 1, "only `b` should transition");
}

// ── BulkAttachToPlan tests ────────────────────────────────────────────────────

#[tokio::test]
async fn bulk_attach_to_plan_attaches_all() {
    let (handler, _tasks_repo, plans_repo) = build_stack().await;
    let project_id = ProjectId::new();
    let plan_id = create_plan(&handler, project_id).await;

    let mut ids = Vec::new();
    for i in 0..4 {
        ids.push(create_task(&handler, &format!("t{i}")).await);
    }

    handler
        .handle(
            Command::BulkAttachToPlan {
                plan_id,
                task_ids: ids.clone(),
            },
            Actor::user(),
        )
        .await
        .unwrap();

    let attached = plans_repo.list_plan_tasks_ordered(plan_id).await.unwrap();
    assert_eq!(attached.len(), ids.len(), "all tasks must be attached");
    let attached_ids: std::collections::HashSet<TaskId> =
        attached.iter().map(|t| t.task_id).collect();
    for id in &ids {
        assert!(attached_ids.contains(id), "task {id} missing from plan");
    }
}

#[tokio::test]
async fn bulk_attach_to_plan_idempotent_on_already_attached() {
    let (handler, _tasks_repo, plans_repo) = build_stack().await;
    let project_id = ProjectId::new();
    let plan_id = create_plan(&handler, project_id).await;
    let a = create_task(&handler, "a").await;
    let b = create_task(&handler, "b").await;

    // Pre-attach `a`.
    handler
        .handle(
            Command::AddPlanTask {
                plan_id,
                task_id: a,
                position: None,
                depends_on: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // Bulk-attach [a, b]: `a` is a no-op, `b` is added — must not panic.
    handler
        .handle(
            Command::BulkAttachToPlan {
                plan_id,
                task_ids: vec![a, b],
            },
            Actor::user(),
        )
        .await
        .unwrap();

    let attached = plans_repo.list_plan_tasks_ordered(plan_id).await.unwrap();
    assert_eq!(attached.len(), 2, "expected exactly 2 distinct attachments");
}

#[tokio::test]
async fn bulk_attach_to_plan_rejects_oversized_request() {
    let (handler, _tasks_repo, _plans) = build_stack().await;
    let project_id = ProjectId::new();
    let plan_id = create_plan(&handler, project_id).await;

    let task_ids: Vec<TaskId> = (0..51).map(|_| TaskId::new()).collect();
    let err = handler
        .handle(
            Command::BulkAttachToPlan { plan_id, task_ids },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)));
}

#[tokio::test]
async fn bulk_attach_to_plan_fails_fast_on_missing_task() {
    let (handler, _tasks_repo, plans_repo) = build_stack().await;
    let project_id = ProjectId::new();
    let plan_id = create_plan(&handler, project_id).await;
    let a = create_task(&handler, "a").await;
    let phantom = TaskId::new();

    let err = handler
        .handle(
            Command::BulkAttachToPlan {
                plan_id,
                task_ids: vec![a, phantom],
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::NotFound(_)));

    // Atomicity: `a` must not have been attached either.
    let attached = plans_repo.list_plan_tasks_ordered(plan_id).await.unwrap();
    assert!(attached.is_empty(), "no tasks should be attached on error");
}

#[tokio::test]
async fn bulk_attach_to_plan_fails_fast_on_missing_plan() {
    let (handler, _tasks_repo, _plans) = build_stack().await;
    let a = create_task(&handler, "a").await;
    let phantom_plan = PlanId::new();

    let err = handler
        .handle(
            Command::BulkAttachToPlan {
                plan_id: phantom_plan,
                task_ids: vec![a],
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::NotFound(_)));
}
