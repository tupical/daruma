//! Unit tests for SetStatus / CompleteTask enforcement and DeleteTask cascade (§3.2 W2.2).
//!
//! Covers:
//! - AC-3: SetStatus(Done) with active blocker → CoreError::Conflict("task_blocked …")
//! - AC-4: Blocker → Done → TaskUnblocked for fully-unblocked downstreams.
//! - Multiple blockers: only unblock when ALL are Done.
//! - No-blocker case: SetStatus(Done) passes cleanly.
//! - CompleteTask with active blocker → same conflict.
//! - DeleteTask cascade: TaskUnlinked per relation + TaskUnblocked for downstreams.

use std::sync::Arc;

use taskagent_core::{Command, CommandHandler};
use taskagent_domain::{Actor, NewTask, RelationKind, Status};
use taskagent_events::{Event, EventBus, EventStore};
use taskagent_shared::{CoreError, TaskId};
use taskagent_storage::{
    ActivityRepo, CommentRepo, Db, ProjectRepo, RelationRepo, SqliteEventStore, TaskRepo,
};

/// Build a CommandHandler wired with TaskRepo + RelationRepo using in-memory SQLite.
async fn build_stack() -> (
    CommandHandler,
    Arc<dyn EventStore>,
    Arc<TaskRepo>,
    Arc<RelationRepo>,
) {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let tasks = Arc::new(TaskRepo::new(pool.clone()));
    let projects = Arc::new(ProjectRepo::new(pool.clone()));
    let comments = Arc::new(CommentRepo::new(pool.clone()));
    let activity = Arc::new(ActivityRepo::new(pool.clone()));
    let relations = Arc::new(RelationRepo::new(pool));
    let bus = EventBus::default();

    let handler = CommandHandler::new(
        store.clone(),
        tasks.clone(),
        projects,
        comments,
        activity,
        bus,
    )
    .with_relations(relations.clone());

    (handler, store, tasks, relations)
}

/// Create a task via handler, return its TaskId.
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

/// Link `from` blocks `to`.
async fn link_blocks(handler: &CommandHandler, from: TaskId, to: TaskId) {
    handler
        .handle(
            Command::LinkTasks {
                from,
                to,
                kind: RelationKind::Blocks,
            },
            Actor::user(),
        )
        .await
        .unwrap();
}

// ── AC-3: SetStatus(Done) with active blocker → conflict ─────────────────────

#[tokio::test]
async fn set_status_done_blocked_by_active() {
    let (handler, ..) = build_stack().await;
    let a = create_task(&handler, "Blocker A").await;
    let b = create_task(&handler, "Blocked B").await;

    // A blocks B; A is still Open (default Inbox status → not Done).
    link_blocks(&handler, a, b).await;

    let err = handler
        .handle(
            Command::SetStatus {
                id: b,
                status: Status::Done,
                force: false,
            },
            Actor::user(),
        )
        .await
        .unwrap_err();

    match err {
        CoreError::Conflict(msg) => {
            assert!(
                msg.contains("task_blocked"),
                "conflict message must contain 'task_blocked', got: {msg}"
            );
        }
        other => panic!("expected Conflict error, got: {other:?}"),
    }
}

// ── AC-4: Blocker → Done → TaskUnblocked emitted for downstream ───────────────

#[tokio::test]
async fn blocker_done_emits_unblocked() {
    let (handler, ..) = build_stack().await;
    let a = create_task(&handler, "Blocker A").await;
    let b = create_task(&handler, "Blocked B").await;

    // A blocks B.
    link_blocks(&handler, a, b).await;

    // Set A to Done → should emit TaskStatusChanged(A, Done) + TaskClosed(A)
    // + TaskUnblocked(B, unblocked_by: A).
    let envs = handler
        .handle(
            Command::SetStatus {
                id: a,
                status: Status::Done,
                force: false,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // Find TaskUnblocked event.
    let unblocked = envs.iter().find(|e| {
        matches!(
            &e.payload,
            Event::TaskUnblocked { task_id, unblocked_by, .. }
            if *task_id == b && *unblocked_by == a
        )
    });
    assert!(
        unblocked.is_some(),
        "expected TaskUnblocked{{task_id: B, unblocked_by: A}} in events, got: {:?}",
        envs.iter().map(|e| &e.payload).collect::<Vec<_>>()
    );

    // Also confirm TaskStatusChanged for A.
    let status_changed = envs.iter().find(|e| {
        matches!(
            &e.payload,
            Event::TaskStatusChanged { task_id, to, .. }
            if *task_id == a && *to == Status::Done
        )
    });
    assert!(
        status_changed.is_some(),
        "expected TaskStatusChanged(A, Done)"
    );
}

// ── Multiple blockers: only unblock B when ALL blockers are Done ──────────────

#[tokio::test]
async fn multiple_blockers_only_unblock_when_all_done() {
    let (handler, ..) = build_stack().await;
    let a = create_task(&handler, "Blocker A").await;
    let c = create_task(&handler, "Blocker C").await;
    let b = create_task(&handler, "Blocked B").await;

    // Both A and C block B.
    link_blocks(&handler, a, b).await;
    link_blocks(&handler, c, b).await;

    // Set A to Done → B still has C as active blocker → no TaskUnblocked.
    let envs_a = handler
        .handle(
            Command::SetStatus {
                id: a,
                status: Status::Done,
                force: false,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    let unblocked_after_a = envs_a.iter().any(|e| {
        matches!(
            &e.payload,
            Event::TaskUnblocked { task_id, .. } if *task_id == b
        )
    });
    assert!(
        !unblocked_after_a,
        "B must NOT be unblocked while C is still active"
    );

    // Now set C to Done → B has no remaining active blockers → TaskUnblocked.
    let envs_c = handler
        .handle(
            Command::SetStatus {
                id: c,
                status: Status::Done,
                force: false,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    let unblocked = envs_c.iter().find(|e| {
        matches!(
            &e.payload,
            Event::TaskUnblocked { task_id, unblocked_by, .. }
            if *task_id == b && *unblocked_by == c
        )
    });
    assert!(
        unblocked.is_some(),
        "expected TaskUnblocked{{B, unblocked_by: C}} after C becomes Done"
    );
}

// ── No-blocker case: SetStatus(Done) passes without TaskUnblocked ─────────────

#[tokio::test]
async fn set_status_done_no_blockers_ok() {
    let (handler, ..) = build_stack().await;
    let b = create_task(&handler, "Free task B").await;

    // B has no blockers → SetStatus(Done) must succeed.
    let envs = handler
        .handle(
            Command::SetStatus {
                id: b,
                status: Status::Done,
                force: false,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // No TaskUnblocked should be emitted.
    let any_unblocked = envs
        .iter()
        .any(|e| matches!(&e.payload, Event::TaskUnblocked { .. }));
    assert!(
        !any_unblocked,
        "no TaskUnblocked expected when task has no blockers"
    );

    // TaskStatusChanged must be present.
    let status_changed = envs.iter().find(|e| {
        matches!(
            &e.payload,
            Event::TaskStatusChanged { task_id, to, .. }
            if *task_id == b && *to == Status::Done
        )
    });
    assert!(
        status_changed.is_some(),
        "expected TaskStatusChanged(B, Done)"
    );
}

// ── CompleteTask with active blocker → conflict ───────────────────────────────

#[tokio::test]
async fn complete_task_blocked_returns_conflict() {
    let (handler, ..) = build_stack().await;
    let a = create_task(&handler, "Blocker A").await;
    let b = create_task(&handler, "Blocked B").await;

    // A blocks B.
    link_blocks(&handler, a, b).await;

    let err = handler
        .handle(Command::CompleteTask { id: b, note: None }, Actor::user())
        .await
        .unwrap_err();

    match err {
        CoreError::Conflict(msg) => {
            assert!(
                msg.contains("task_blocked"),
                "conflict message must contain 'task_blocked', got: {msg}"
            );
        }
        other => panic!("expected Conflict error, got: {other:?}"),
    }
}

// ── DeleteTask cascade: TaskUnlinked per relation + TaskUnblocked ─────────────

#[tokio::test]
async fn delete_task_emits_unlinked_per_relation() {
    let (handler, _store, ..) = build_stack().await;
    let a = create_task(&handler, "Task A (to be deleted)").await;
    let b = create_task(&handler, "Task B (blocked by A)").await;
    let c = create_task(&handler, "Task C (relates to A)").await;

    // A blocks B; A relates_to C.
    link_blocks(&handler, a, b).await;
    handler
        .handle(
            Command::LinkTasks {
                from: a,
                to: c,
                kind: RelationKind::RelatesTo,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // Delete A → should emit 2× TaskUnlinked (one per relation) +
    // TaskUnblocked{B, unblocked_by: A} (B's only blocker was A) + TaskDeleted.
    let envs = handler
        .handle(Command::DeleteTask { id: a }, Actor::user())
        .await
        .unwrap();

    let unlinked_events: Vec<_> = envs
        .iter()
        .filter(|e| matches!(&e.payload, Event::TaskUnlinked { .. }))
        .collect();
    assert_eq!(
        unlinked_events.len(),
        2,
        "expected 2 TaskUnlinked events (one per relation), got {}",
        unlinked_events.len()
    );

    let unblocked = envs.iter().find(|e| {
        matches!(
            &e.payload,
            Event::TaskUnblocked { task_id, unblocked_by, .. }
            if *task_id == b && *unblocked_by == a
        )
    });
    assert!(
        unblocked.is_some(),
        "expected TaskUnblocked{{B, unblocked_by: A}} when A (sole blocker) is deleted"
    );

    let deleted = envs
        .iter()
        .find(|e| matches!(&e.payload, Event::TaskDeleted { task_id } if *task_id == a));
    assert!(deleted.is_some(), "expected TaskDeleted for A");
}
