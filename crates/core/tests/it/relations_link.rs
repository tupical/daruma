//! Unit tests for LinkTasks / UnlinkTasks command handling (§3.2 W2.1).

use std::sync::Arc;

use daruma_core::{Command, CommandHandler};
use daruma_domain::{Actor, NewTask, RelationKind};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::{CoreError, RelationId, TaskId};
use daruma_storage::{
    ActivityRepo, CommentRepo, Db, ProjectRepo, RelationRepo, SqliteEventStore, TaskRepo,
};

/// Build a CommandHandler wired with a RelationRepo.
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

// ── link_tasks_emits_task_linked ──────────────────────────────────────────────

#[tokio::test]
async fn link_tasks_emits_task_linked() {
    let (handler, _store, _tasks, _relations) = build_stack().await;
    let a = create_task(&handler, "Task A").await;
    let b = create_task(&handler, "Task B").await;

    let envs = handler
        .handle(
            Command::LinkTasks {
                from: a,
                to: b,
                kind: RelationKind::Blocks,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    assert_eq!(envs.len(), 1, "exactly 1 event: TaskLinked");
    match &envs[0].payload {
        Event::TaskLinked { from, to, kind, .. } => {
            assert_eq!(*from, a);
            assert_eq!(*to, b);
            assert_eq!(*kind, RelationKind::Blocks);
        }
        other => panic!("expected TaskLinked, got: {other:?}"),
    }
}

// ── unlink_tasks_emits_task_unlinked ─────────────────────────────────────────

#[tokio::test]
async fn unlink_tasks_emits_task_unlinked() {
    let (handler, _store, _tasks, _relations) = build_stack().await;
    let a = create_task(&handler, "Task A").await;
    let b = create_task(&handler, "Task B").await;

    // First link them.
    let link_envs = handler
        .handle(
            Command::LinkTasks {
                from: a,
                to: b,
                kind: RelationKind::RelatesTo,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    let relation_id = match &link_envs[0].payload {
        Event::TaskLinked { relation_id, .. } => *relation_id,
        _ => panic!("expected TaskLinked"),
    };

    // Now unlink.
    let unlink_envs = handler
        .handle(Command::UnlinkTasks { id: relation_id }, Actor::user())
        .await
        .unwrap();

    assert_eq!(unlink_envs.len(), 1, "exactly 1 event: TaskUnlinked");
    match &unlink_envs[0].payload {
        Event::TaskUnlinked {
            relation_id: rid,
            from,
            to,
            kind,
            ..
        } => {
            assert_eq!(*rid, relation_id);
            assert_eq!(*from, a);
            assert_eq!(*to, b);
            assert_eq!(*kind, RelationKind::RelatesTo);
        }
        other => panic!("expected TaskUnlinked, got: {other:?}"),
    }
}

// ── link_duplicate_returns_relation_exists_conflict ───────────────────────────

#[tokio::test]
async fn link_duplicate_returns_relation_exists_conflict() {
    let (handler, _store, _tasks, _relations) = build_stack().await;
    let a = create_task(&handler, "Task A").await;
    let b = create_task(&handler, "Task B").await;

    // First link — OK.
    handler
        .handle(
            Command::LinkTasks {
                from: a,
                to: b,
                kind: RelationKind::Blocks,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // Second link with same (from, to, kind) — must return Conflict(relation_exists).
    let err = handler
        .handle(
            Command::LinkTasks {
                from: a,
                to: b,
                kind: RelationKind::Blocks,
            },
            Actor::user(),
        )
        .await
        .unwrap_err();

    match err {
        CoreError::Conflict(msg) => {
            assert!(
                msg.contains("relation_exists"),
                "expected relation_exists in conflict message, got: {msg}"
            );
        }
        other => panic!("expected Conflict error, got: {other:?}"),
    }
}

// ── unlink_blocks_unblocks ───────────────────────────────────────────────────

async fn link_blocks(handler: &CommandHandler, from: TaskId, to: TaskId) -> RelationId {
    let envs = handler
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
    match &envs[0].payload {
        Event::TaskLinked { relation_id, .. } => *relation_id,
        other => panic!("expected TaskLinked, got: {other:?}"),
    }
}

#[tokio::test]
async fn unlink_of_last_blocker_emits_task_unblocked() {
    let (handler, _store, _tasks, _relations) = build_stack().await;
    let blocker = create_task(&handler, "Blocker").await;
    let other = create_task(&handler, "Other blocker").await;
    let blocked = create_task(&handler, "Blocked").await;
    let first = link_blocks(&handler, blocker, blocked).await;
    let second = link_blocks(&handler, other, blocked).await;

    // `other` still blocks: the task is not free yet.
    let envs = handler
        .handle(Command::UnlinkTasks { id: first }, Actor::user())
        .await
        .unwrap();
    assert_eq!(envs.len(), 1, "exactly 1 event: TaskUnlinked");

    let envs = handler
        .handle(Command::UnlinkTasks { id: second }, Actor::user())
        .await
        .unwrap();
    assert_eq!(envs.len(), 2, "TaskUnlinked and TaskUnblocked");
    assert!(matches!(envs[0].payload, Event::TaskUnlinked { .. }));
    match &envs[1].payload {
        Event::TaskUnblocked {
            task_id,
            unblocked_by,
            ..
        } => {
            assert_eq!(*task_id, blocked);
            assert_eq!(*unblocked_by, other);
        }
        other => panic!("expected TaskUnblocked, got: {other:?}"),
    }
}

async fn complete_task(handler: &CommandHandler, id: TaskId) {
    handler
        .handle(
            Command::CompleteTask { id, note: None },
            Actor::user(),
        )
        .await
        .unwrap();
}

/// An edge linked from a task that is already Done never held anything back,
/// so removing it is no unblock: nothing went from blocked to ready.
#[tokio::test]
async fn unlink_of_done_blocker_emits_no_task_unblocked() {
    let (handler, _store, _tasks, _relations) = build_stack().await;
    let done = create_task(&handler, "Finished first").await;
    let blocked = create_task(&handler, "Linked later").await;
    complete_task(&handler, done).await;
    let edge = link_blocks(&handler, done, blocked).await;

    let envs = handler
        .handle(Command::UnlinkTasks { id: edge }, Actor::user())
        .await
        .unwrap();
    assert_eq!(envs.len(), 1, "exactly 1 event: TaskUnlinked");
    assert!(matches!(envs[0].payload, Event::TaskUnlinked { .. }));
}

/// The same edge next to a live blocker: the Done one goes first and frees
/// nothing, the live one goes last and is the real unblock.
#[tokio::test]
async fn unlink_of_last_live_blocker_after_done_one_emits_task_unblocked() {
    let (handler, _store, _tasks, _relations) = build_stack().await;
    let done = create_task(&handler, "Finished first").await;
    let live = create_task(&handler, "Still open").await;
    let blocked = create_task(&handler, "Blocked").await;
    complete_task(&handler, done).await;
    let done_edge = link_blocks(&handler, done, blocked).await;
    let live_edge = link_blocks(&handler, live, blocked).await;

    let envs = handler
        .handle(Command::UnlinkTasks { id: done_edge }, Actor::user())
        .await
        .unwrap();
    assert_eq!(envs.len(), 1, "exactly 1 event: TaskUnlinked");

    let envs = handler
        .handle(Command::UnlinkTasks { id: live_edge }, Actor::user())
        .await
        .unwrap();
    assert_eq!(envs.len(), 2, "TaskUnlinked and TaskUnblocked");
    match &envs[1].payload {
        Event::TaskUnblocked {
            task_id,
            unblocked_by,
            ..
        } => {
            assert_eq!(*task_id, blocked);
            assert_eq!(*unblocked_by, live);
        }
        other => panic!("expected TaskUnblocked, got: {other:?}"),
    }
}

/// `DeleteTask` shares the helper, so a Done blocker going away frees no one.
#[tokio::test]
async fn delete_of_done_blocker_emits_no_task_unblocked() {
    let (handler, _store, _tasks, _relations) = build_stack().await;
    let done = create_task(&handler, "Finished first").await;
    let blocked = create_task(&handler, "Linked later").await;
    complete_task(&handler, done).await;
    link_blocks(&handler, done, blocked).await;

    let envs = handler
        .handle(Command::DeleteTask { id: done }, Actor::user())
        .await
        .unwrap();
    assert!(
        !envs
            .iter()
            .any(|env| matches!(env.payload, Event::TaskUnblocked { .. })),
        "{envs:?}"
    );
}

// ── unlink_nonexistent_returns_not_found ──────────────────────────────────────

#[tokio::test]
async fn unlink_nonexistent_returns_not_found() {
    let (handler, ..) = build_stack().await;
    let bogus_id = RelationId::new();

    let err = handler
        .handle(Command::UnlinkTasks { id: bogus_id }, Actor::user())
        .await
        .unwrap_err();

    assert!(
        matches!(err, CoreError::NotFound(_)),
        "expected NotFound, got: {err:?}"
    );
}

// ── link_tasks_cycle_is_rejected ─────────────────────────────────────────────

#[tokio::test]
async fn link_tasks_cycle_is_rejected() {
    let (handler, _store, _tasks, _relations) = build_stack().await;
    let a = create_task(&handler, "Task A").await;
    let b = create_task(&handler, "Task B").await;

    // A blocks B.
    handler
        .handle(
            Command::LinkTasks {
                from: a,
                to: b,
                kind: RelationKind::Blocks,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // B blocks A → cycle.
    let err = handler
        .handle(
            Command::LinkTasks {
                from: b,
                to: a,
                kind: RelationKind::Blocks,
            },
            Actor::user(),
        )
        .await
        .unwrap_err();

    match err {
        CoreError::Validation(msg) => {
            assert!(
                msg.contains("cycle_detected"),
                "expected cycle_detected, got: {msg}"
            );
        }
        other => panic!("expected Validation error, got: {other:?}"),
    }
}
