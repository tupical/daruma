//! Unit tests for §3.7.2 / LIN A.3 — `RelationKind::WasBlocking`.
//!
//! When a blocker transitions to `Status::Done`, every active `Blocks` edge
//! outgoing from that task is transitioned to `WasBlocking`:
//!   - A new `TaskRelationKindChanged` event is emitted in the same batch as
//!     `TaskUnblocked` for the downstream task.
//!   - The underlying `task_relations` row's `kind` is flipped from `'blocks'`
//!     to `'was_blocking'`, so it no longer surfaces via `list_blockers` /
//!     `list_blocks_targets` (audit-only retention).
//!   - The downstream task still sees exactly one `TaskUnblocked` event.

use std::sync::Arc;

use taskagent_core::{Command, CommandHandler};
use taskagent_domain::{Actor, NewTask, RelationKind, Status};
use taskagent_events::{Event, EventBus, EventStore};
use taskagent_shared::TaskId;
use taskagent_storage::{
    ActivityRepo, CommentRepo, Db, ProjectRepo, RelationRepo, SqliteEventStore, TaskRepo,
};

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

// ── AC §3.7.2: SetStatus(blocker → Done) flips Blocks → WasBlocking ──────────

#[tokio::test]
async fn set_status_done_transitions_blocks_to_was_blocking() {
    let (handler, _store, _tasks, relations) = build_stack().await;
    let a = create_task(&handler, "Blocker A").await;
    let b = create_task(&handler, "Blocked B").await;
    link_blocks(&handler, a, b).await;

    // Sanity: before A → Done, list_blockers(B) shows A.
    let before = relations.list_blockers(b).await.unwrap();
    assert_eq!(before.len(), 1, "B has one active blocker before A is Done");
    assert_eq!(before[0].kind, RelationKind::Blocks);

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

    // Exactly one TaskRelationKindChanged event for the A→B edge:
    //   Blocks → WasBlocking.
    let transitions: Vec<_> = envs
        .iter()
        .filter_map(|e| match &e.payload {
            Event::TaskRelationKindChanged {
                from,
                to,
                from_kind,
                to_kind,
                ..
            } => Some((*from, *to, *from_kind, *to_kind)),
            _ => None,
        })
        .collect();
    assert_eq!(
        transitions.len(),
        1,
        "expected exactly one TaskRelationKindChanged, got: {:?}",
        envs.iter().map(|e| e.payload.kind()).collect::<Vec<_>>()
    );
    let (from, to, from_kind, to_kind) = transitions[0];
    assert_eq!(from, a);
    assert_eq!(to, b);
    assert_eq!(from_kind, RelationKind::Blocks);
    assert_eq!(to_kind, RelationKind::WasBlocking);

    // Exactly one TaskUnblocked for B.
    let unblocked_count = envs
        .iter()
        .filter(|e| matches!(&e.payload, Event::TaskUnblocked { task_id, .. } if *task_id == b))
        .count();
    assert_eq!(unblocked_count, 1, "exactly one TaskUnblocked for B");

    // After: list_blockers(B) is empty — the row is no longer an active blocker.
    let after = relations.list_blockers(b).await.unwrap();
    assert!(
        after.is_empty(),
        "B should have no active blockers after Blocks → WasBlocking transition, got: {after:?}"
    );

    // The row still exists in the table (audit retention) with kind == WasBlocking.
    let all_for_b = relations.list_by_task(b).await.unwrap();
    let was = all_for_b
        .iter()
        .find(|r| r.from == a && r.to == b)
        .expect("the historical relation row must still exist");
    assert_eq!(was.kind, RelationKind::WasBlocking);
}

// ── AC §3.7.2: CompleteTask path also transitions Blocks → WasBlocking ───────

#[tokio::test]
async fn complete_task_transitions_blocks_to_was_blocking() {
    let (handler, _store, _tasks, relations) = build_stack().await;
    let a = create_task(&handler, "Blocker A").await;
    let b = create_task(&handler, "Blocked B").await;
    link_blocks(&handler, a, b).await;

    let envs = handler
        .handle(Command::CompleteTask { id: a }, Actor::user())
        .await
        .unwrap();

    let saw_transition = envs.iter().any(|e| {
        matches!(
            &e.payload,
            Event::TaskRelationKindChanged {
                from,
                to,
                from_kind: RelationKind::Blocks,
                to_kind: RelationKind::WasBlocking,
                ..
            } if *from == a && *to == b
        )
    });
    assert!(
        saw_transition,
        "CompleteTask must emit TaskRelationKindChanged(Blocks → WasBlocking) for outgoing edge"
    );

    let row = relations
        .list_by_task(b)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.from == a && r.to == b)
        .expect("row retained for audit");
    assert_eq!(row.kind, RelationKind::WasBlocking);
}

// ── Multiple blockers: each Blocks edge owned by the resolving task transitions ──

#[tokio::test]
async fn transition_only_for_resolving_blocker_edges() {
    let (handler, _store, _tasks, relations) = build_stack().await;
    let a = create_task(&handler, "Blocker A").await;
    let c = create_task(&handler, "Blocker C").await;
    let b = create_task(&handler, "Blocked B").await;
    link_blocks(&handler, a, b).await;
    link_blocks(&handler, c, b).await;

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

    // Only the A→B edge transitions. C→B remains Blocks.
    let transitions: Vec<_> = envs
        .iter()
        .filter_map(|e| match &e.payload {
            Event::TaskRelationKindChanged { from, to, .. } => Some((*from, *to)),
            _ => None,
        })
        .collect();
    assert_eq!(
        transitions,
        vec![(a, b)],
        "only A's outgoing edge transitions"
    );

    // No TaskUnblocked yet (C is still an active blocker).
    let unblocked = envs
        .iter()
        .any(|e| matches!(&e.payload, Event::TaskUnblocked { .. }));
    assert!(!unblocked, "B must not be unblocked while C is active");

    // C→B is still an active Blocks edge.
    let blockers = relations.list_blockers(b).await.unwrap();
    assert_eq!(blockers.len(), 1);
    assert_eq!(blockers[0].from, c);
    assert_eq!(blockers[0].kind, RelationKind::Blocks);
}
