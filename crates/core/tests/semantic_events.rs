//! Wave 2 / W2.1 — semantic-events integration tests.
//!
//! Verifies that `Command::SetStatus` and `Command::AddComment` emit the
//! semantic envelopes (`TaskReopened` / `TaskClosed` / `TaskCommented`) in
//! addition to the mechanical ones — i.e. AC-2 from
//! `.omc/plans/section-e-multi-agent-realtime.md`.

use std::sync::Arc;

use taskagent_core::{Command, CommandHandler};
use taskagent_domain::{Actor, NewComment, NewTask, Status};
use taskagent_events::{Event, EventBus, EventEnvelope, EventStore};
use taskagent_shared::TaskId;
use taskagent_storage::{ActivityRepo, CommentRepo, Db, ProjectRepo, SqliteEventStore, TaskRepo};

async fn build_handler() -> CommandHandler {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let tasks = Arc::new(TaskRepo::new(pool.clone()));
    let projects = Arc::new(ProjectRepo::new(pool.clone()));
    let comments = Arc::new(CommentRepo::new(pool.clone()));
    let activity = Arc::new(ActivityRepo::new(pool));
    let bus = EventBus::default();
    CommandHandler::new(store, tasks, projects, comments, activity, bus)
}

async fn create_task(handler: &CommandHandler, status: Option<Status>) -> TaskId {
    let mut nt = NewTask::new("semantic-events task");
    nt.status = status;
    let envs = handler
        .handle(Command::CreateTask { task: nt }, Actor::user())
        .await
        .unwrap();
    match &envs[0].payload {
        Event::TaskCreated { task } => task.id.unwrap(),
        _ => panic!("expected TaskCreated"),
    }
}

fn kinds(envs: &[EventEnvelope]) -> Vec<&'static str> {
    envs.iter().map(|e| e.payload.kind()).collect()
}

// ── AC-2: terminal → non-terminal emits TaskReopened ──────────────────────────

#[tokio::test]
async fn set_status_done_to_todo_emits_task_reopened() {
    let handler = build_handler().await;
    let id = create_task(&handler, Some(Status::Done)).await;

    let envs = handler
        .handle(
            Command::SetStatus {
                id,
                status: Status::Todo,
                force: false,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    assert_eq!(kinds(&envs), vec!["task_status_changed", "task_reopened"]);

    // Make sure the semantic event carries the right task and actor.
    match &envs[1].payload {
        Event::TaskReopened { task_id, by, .. } => {
            assert_eq!(*task_id, id);
            assert!(matches!(by, Actor::User));
        }
        other => panic!("expected TaskReopened, got {other:?}"),
    }
}

// ── AC-2: non-terminal → terminal emits TaskClosed ────────────────────────────

#[tokio::test]
async fn set_status_todo_to_done_emits_task_closed() {
    let handler = build_handler().await;
    let id = create_task(&handler, Some(Status::Todo)).await;

    let envs = handler
        .handle(
            Command::SetStatus {
                id,
                status: Status::Done,
                force: false,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    assert_eq!(kinds(&envs), vec!["task_status_changed", "task_closed"]);

    match &envs[1].payload {
        Event::TaskClosed { task_id, .. } => assert_eq!(*task_id, id),
        other => panic!("expected TaskClosed, got {other:?}"),
    }
}

// ── AC-2: non-terminal → non-terminal emits only the mechanical event ─────────

#[tokio::test]
async fn set_status_todo_to_in_progress_emits_only_mechanical() {
    let handler = build_handler().await;
    let id = create_task(&handler, Some(Status::Todo)).await;

    let envs = handler
        .handle(
            Command::SetStatus {
                id,
                status: Status::InProgress,
                force: false,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    assert_eq!(kinds(&envs), vec!["task_status_changed"]);
}

// ── AC-2 (extension): AddComment emits TaskCommented with preview ─────────────

#[tokio::test]
async fn add_comment_emits_task_commented_with_preview() {
    let handler = build_handler().await;
    let task_id = TaskId::new();

    // 100 characters; preview must be capped at 80.
    let body = "a".repeat(100);

    let envs = handler
        .handle(
            Command::AddComment {
                comment: NewComment {
                    id: None,
                    task_id,
                    body: body.clone(),
                    parent_id: None,
                    kind: None,
                },
            },
            Actor::user(),
        )
        .await
        .unwrap();

    assert_eq!(kinds(&envs), vec!["comment_added", "task_commented"]);

    let comment_id = match &envs[0].payload {
        Event::CommentAdded { comment } => comment.id,
        other => panic!("expected CommentAdded, got {other:?}"),
    };

    match &envs[1].payload {
        Event::TaskCommented {
            task_id: t,
            comment_id: c,
            preview,
            ..
        } => {
            assert_eq!(*t, task_id);
            assert_eq!(*c, comment_id);
            assert_eq!(preview.chars().count(), 80);
            assert!(preview.chars().all(|ch| ch == 'a'));
        }
        other => panic!("expected TaskCommented, got {other:?}"),
    }
}

// ── AC-2: CompleteTask also emits TaskClosed ──────────────────────────────────

#[tokio::test]
async fn complete_task_emits_task_closed() {
    let handler = build_handler().await;
    let id = create_task(&handler, Some(Status::InProgress)).await;

    let envs = handler
        .handle(Command::CompleteTask { id, note: None }, Actor::user())
        .await
        .unwrap();

    assert_eq!(
        kinds(&envs),
        vec!["task_status_changed", "task_completed", "task_closed"]
    );
}
