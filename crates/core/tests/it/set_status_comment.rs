//! `SetStatus.comment` — a note recorded atomically with a status transition
//! (Action Fusion for the `daruma_comment` → `daruma_set_status` pair).
//!
//! The contract under test: the comment exists iff the transition landed.
//! A lifecycle-gate block, a relation block, a no-op transition or invalid
//! note text all leave the comment table untouched.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use daruma_core::lifecycle_gate::{
    GateCheck, GateDecision, GateOverride, LifecycleGate, TriggerEvent,
};
use daruma_core::{Command, CommandHandler};
use daruma_domain::{Actor, CommentKind, NewTask, RelationKind, Status, TransitionComment};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::TaskId;
use daruma_storage::{
    ActivityRepo, CommentRepo, Db, ProjectRepo, RelationRepo, SqliteEventStore, TaskRepo,
};

#[derive(Default)]
struct BlockGate {
    block: Option<TriggerEvent>,
    seen: Mutex<Vec<TriggerEvent>>,
}

#[async_trait]
impl LifecycleGate for BlockGate {
    async fn check(
        &self,
        _actor: &Actor,
        check: &GateCheck,
        _gate_override: &GateOverride,
    ) -> daruma_shared::Result<GateDecision> {
        self.seen.lock().unwrap().push(check.trigger);
        if self.block == Some(check.trigger) {
            return Ok(GateDecision::Blocked {
                message: format!("{} requires evidence", check.trigger.as_str()),
                details: serde_json::Value::Null,
            });
        }
        Ok(GateDecision::Allowed)
    }
}

struct Stack {
    handler: CommandHandler,
    store: Arc<dyn EventStore>,
    tasks: Arc<TaskRepo>,
    comments: Arc<CommentRepo>,
}

async fn stack(gate: Option<Arc<BlockGate>>) -> Stack {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let tasks = Arc::new(TaskRepo::new(pool.clone()));
    let projects = Arc::new(ProjectRepo::new(pool.clone()));
    let comments = Arc::new(CommentRepo::new(pool.clone()));
    let activity = Arc::new(ActivityRepo::new(pool.clone()));
    let relations = Arc::new(RelationRepo::new(pool));
    let mut handler = CommandHandler::new(
        store.clone(),
        tasks.clone(),
        projects,
        comments.clone(),
        activity,
        EventBus::default(),
    )
    .with_relations(relations);
    if let Some(gate) = gate {
        handler = handler.with_lifecycle_gate(gate);
    }
    Stack {
        handler,
        store,
        tasks,
        comments,
    }
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
        other => panic!("expected TaskCreated, got {other:?}"),
    }
}

fn set_status(id: TaskId, status: Status, body: &str, kind: Option<CommentKind>) -> Command {
    Command::SetStatus {
        id,
        status,
        force: false,
        override_reason: None,
        comment: Some(TransitionComment {
            body: body.to_string(),
            kind,
        }),
    }
}

#[tokio::test]
async fn comment_lands_with_the_transition() {
    let s = stack(None).await;
    let task = create_task(&s.handler, "Fused").await;
    let before = s.store.load_since(0, 1000).await.unwrap().len();

    let envs = s
        .handler
        .handle(
            set_status(
                task,
                Status::InProgress,
                "  starting on the parser  ",
                Some(CommentKind::Intent),
            ),
            Actor::user(),
        )
        .await
        .unwrap();

    let kinds: Vec<&str> = envs
        .iter()
        .map(|e| match &e.payload {
            Event::TaskStatusChanged { .. } => "status",
            Event::CommentAdded { .. } => "comment",
            Event::TaskCommented { .. } => "commented",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, vec!["status", "comment", "commented"], "{envs:?}");

    let stored = s.comments.list_for_task(task).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].body, "starting on the parser", "body is trimmed");
    assert_eq!(stored[0].kind, Some(CommentKind::Intent));
    assert_eq!(
        s.tasks.get(task).await.unwrap().unwrap().status,
        Status::InProgress
    );
    assert_eq!(s.store.load_since(0, 1000).await.unwrap().len(), before + 3);
}

#[tokio::test]
async fn gate_block_records_nothing() {
    let gate = Arc::new(BlockGate {
        block: Some(TriggerEvent::TaskBeforeComplete),
        ..BlockGate::default()
    });
    let s = stack(Some(gate.clone())).await;
    let task = create_task(&s.handler, "Gated").await;
    let before = s.store.load_since(0, 1000).await.unwrap().len();

    let err = s
        .handler
        .handle(
            set_status(
                task,
                Status::Done,
                "done, honest",
                Some(CommentKind::Outcome),
            ),
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("rule_blocked"), "{err}");
    assert!(gate
        .seen
        .lock()
        .unwrap()
        .contains(&TriggerEvent::TaskBeforeComplete));

    assert!(
        s.comments.list_for_task(task).await.unwrap().is_empty(),
        "comment must not survive a blocked transition"
    );
    assert_eq!(
        s.tasks.get(task).await.unwrap().unwrap().status,
        Status::Inbox
    );
    assert_eq!(
        s.store.load_since(0, 1000).await.unwrap().len(),
        before,
        "no mutation events persisted"
    );
}

#[tokio::test]
async fn relation_block_records_nothing() {
    let s = stack(None).await;
    let blocker = create_task(&s.handler, "Blocker").await;
    let blocked = create_task(&s.handler, "Blocked").await;
    s.handler
        .handle(
            Command::LinkTasks {
                from: blocker,
                to: blocked,
                kind: RelationKind::Blocks,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    let err = s
        .handler
        .handle(
            set_status(blocked, Status::Done, "closing anyway", None),
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("task_blocked"), "{err}");
    assert!(s.comments.list_for_task(blocked).await.unwrap().is_empty());
    assert_eq!(
        s.tasks.get(blocked).await.unwrap().unwrap().status,
        Status::Inbox
    );
}

#[tokio::test]
async fn noop_transition_records_no_comment() {
    let s = stack(None).await;
    let task = create_task(&s.handler, "Already inbox").await;
    let envs = s
        .handler
        .handle(
            set_status(task, Status::Inbox, "nothing changed", None),
            Actor::user(),
        )
        .await
        .unwrap();
    assert!(envs.is_empty(), "{envs:?}");
    assert!(s.comments.list_for_task(task).await.unwrap().is_empty());
}

#[tokio::test]
async fn invalid_comment_rejects_the_whole_command() {
    let s = stack(None).await;
    let task = create_task(&s.handler, "Validated").await;

    for body in ["   ", &"x".repeat(TransitionComment::MAX_BODY_BYTES + 1)] {
        let err = s
            .handler
            .handle(set_status(task, Status::Todo, body, None), Actor::user())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("transition comment body"), "{err}");
        assert_eq!(
            s.tasks.get(task).await.unwrap().unwrap().status,
            Status::Inbox,
            "transition must not apply when its comment is invalid"
        );
        assert!(s.comments.list_for_task(task).await.unwrap().is_empty());
    }

    // Exactly at the limit is fine.
    s.handler
        .handle(
            set_status(
                task,
                Status::Todo,
                &"y".repeat(TransitionComment::MAX_BODY_BYTES),
                None,
            ),
            Actor::user(),
        )
        .await
        .unwrap();
    assert_eq!(s.comments.list_for_task(task).await.unwrap().len(), 1);
}

#[tokio::test]
async fn legacy_wire_shape_without_comment_still_parses() {
    let cmd: Command = serde_json::from_value(serde_json::json!({
        "type": "set_status", "id": TaskId::new(), "status": "todo"
    }))
    .unwrap();
    assert!(matches!(cmd, Command::SetStatus { comment: None, .. }));

    let cmd: Command = serde_json::from_value(serde_json::json!({
        "type": "set_status", "id": TaskId::new(), "status": "done",
        "comment": {"body": "shipped", "kind": "outcome"}
    }))
    .unwrap();
    match cmd {
        Command::SetStatus {
            comment: Some(c), ..
        } => {
            assert_eq!(c.body, "shipped");
            assert_eq!(c.kind, Some(CommentKind::Outcome));
        }
        other => panic!("{other:?}"),
    }
}
