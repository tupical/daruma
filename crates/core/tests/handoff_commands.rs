//! P5 — handoff contract commands (request / accept / reject).
//!
//! Covers: endpoint validation (unknown units, self-handoff), the
//! open → accepted and open → rejected → re-request(reopen) lifecycles,
//! idempotent no-ops, and the "accepted pair cannot be reopened" conflict.

use std::sync::Arc;

use daruma_core::{Command, CommandHandler};
use daruma_domain::{Actor, HandoffStatus, NewHandoffContract, NewTask, NewWorkUnit};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::{CoreError, TaskId, WorkUnitId};
use daruma_storage::{
    ActivityRepo, CommentRepo, Db, HandoffRepo, ProjectRepo, SqliteEventStore, TaskRepo,
    WorkUnitRepo,
};

async fn build_stack() -> (CommandHandler, Arc<HandoffRepo>) {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let tasks = Arc::new(TaskRepo::new(pool.clone()));
    let projects = Arc::new(ProjectRepo::new(pool.clone()));
    let comments = Arc::new(CommentRepo::new(pool.clone()));
    let activity = Arc::new(ActivityRepo::new(pool.clone()));
    let work_units = Arc::new(WorkUnitRepo::new(pool.clone()));
    let handoffs = Arc::new(HandoffRepo::new(pool));
    let bus = EventBus::default();

    let handler = CommandHandler::new(store, tasks, projects, comments, activity, bus)
        .with_work_units(work_units)
        .with_handoffs(handoffs.clone());
    (handler, handoffs)
}

async fn create_task(handler: &CommandHandler) -> TaskId {
    let envs = handler
        .handle(
            Command::CreateTask {
                task: NewTask::new("parent"),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    envs.iter()
        .find_map(|e| match &e.payload {
            Event::TaskCreated { task } => task.id,
            _ => None,
        })
        .expect("TaskCreated")
}

async fn create_unit(handler: &CommandHandler, task_id: TaskId, title: &str) -> WorkUnitId {
    let envs = handler
        .handle(
            Command::CreateWorkUnit {
                work_unit: NewWorkUnit {
                    id: None,
                    task_id,
                    stage_plan_id: None,
                    title: title.into(),
                    description: None,
                    status: None,
                    priority: None,
                    capability_tags: vec![],
                    artifact_refs: vec![],
                    acceptance: vec![],
                },
            },
            Actor::user(),
        )
        .await
        .unwrap();
    envs.iter()
        .find_map(|e| match &e.payload {
            Event::WorkUnitCreated { work_unit } => Some(work_unit.id),
            _ => None,
        })
        .expect("WorkUnitCreated")
}

fn new_handoff(from: WorkUnitId, to: WorkUnitId) -> NewHandoffContract {
    NewHandoffContract {
        from_work_unit_id: from,
        to_work_unit_id: to,
        required_artifact_ids: vec!["artifact://api/dashboard@v1".into()],
        required_state: Some("approved".into()),
        checklist: vec!["contract published".into()],
        owner_agent_id: None,
    }
}

#[tokio::test]
async fn request_validates_endpoints() {
    let (handler, _handoffs) = build_stack().await;
    let task = create_task(&handler).await;
    let real = create_unit(&handler, task, "producer").await;

    // Unknown consumer unit → NotFound.
    let err = handler
        .handle(
            Command::RequestHandoff {
                handoff: new_handoff(real, WorkUnitId::new()),
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::NotFound { .. }), "got: {err:?}");

    // Self-handoff → validation error.
    let err = handler
        .handle(
            Command::RequestHandoff {
                handoff: new_handoff(real, real),
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Validation { .. }), "got: {err:?}");
}

#[tokio::test]
async fn open_accept_lifecycle_with_noop_and_conflict() {
    let (handler, handoffs) = build_stack().await;
    let task = create_task(&handler).await;
    let from = create_unit(&handler, task, "producer").await;
    let to = create_unit(&handler, task, "consumer").await;

    let envs = handler
        .handle(
            Command::RequestHandoff {
                handoff: new_handoff(from, to),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let handoff_id = envs
        .iter()
        .find_map(|e| match &e.payload {
            Event::HandoffRequested { handoff } => Some(handoff.id),
            _ => None,
        })
        .expect("HandoffRequested");
    assert_eq!(
        handoffs.get(handoff_id).await.unwrap().unwrap().status,
        HandoffStatus::Open
    );

    handler
        .handle(
            Command::AcceptHandoff {
                handoff_id,
                notes: Some("complete".into()),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    assert_eq!(
        handoffs.get(handoff_id).await.unwrap().unwrap().status,
        HandoffStatus::Accepted
    );

    // Re-accept is a no-op; rejecting an accepted handoff is a conflict.
    let envs = handler
        .handle(
            Command::AcceptHandoff {
                handoff_id,
                notes: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    assert!(envs.is_empty(), "re-accept is a no-op");
    let err = handler
        .handle(
            Command::RejectHandoff {
                handoff_id,
                reason: "too late".into(),
                required_changes: vec![],
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Conflict { .. }), "got: {err:?}");

    // The accepted pair cannot be silently reopened.
    let err = handler
        .handle(
            Command::RequestHandoff {
                handoff: new_handoff(from, to),
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Conflict { .. }), "got: {err:?}");
}

#[tokio::test]
async fn reject_then_rerequest_reopens_same_contract() {
    let (handler, handoffs) = build_stack().await;
    let task = create_task(&handler).await;
    let from = create_unit(&handler, task, "producer").await;
    let to = create_unit(&handler, task, "consumer").await;

    let envs = handler
        .handle(
            Command::RequestHandoff {
                handoff: new_handoff(from, to),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let handoff_id = envs
        .iter()
        .find_map(|e| match &e.payload {
            Event::HandoffRequested { handoff } => Some(handoff.id),
            _ => None,
        })
        .unwrap();

    // Empty rejection reason is rejected.
    let err = handler
        .handle(
            Command::RejectHandoff {
                handoff_id,
                reason: "   ".into(),
                required_changes: vec![],
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Validation { .. }), "got: {err:?}");

    handler
        .handle(
            Command::RejectHandoff {
                handoff_id,
                reason: "missing error cases".into(),
                required_changes: vec!["add 4xx handling".into()],
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let contract = handoffs.get(handoff_id).await.unwrap().unwrap();
    assert_eq!(contract.status, HandoffStatus::Rejected);
    assert_eq!(contract.required_changes, vec!["add 4xx handling"]);

    // Re-request reopens the SAME contract id.
    let envs = handler
        .handle(
            Command::RequestHandoff {
                handoff: new_handoff(from, to),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let reopened = envs
        .iter()
        .find_map(|e| match &e.payload {
            Event::HandoffRequested { handoff } => Some(handoff.id),
            _ => None,
        })
        .unwrap();
    assert_eq!(reopened, handoff_id, "re-request reuses the contract id");
    let contract = handoffs.get(handoff_id).await.unwrap().unwrap();
    assert_eq!(contract.status, HandoffStatus::Open);
    assert!(contract.required_changes.is_empty());
}
