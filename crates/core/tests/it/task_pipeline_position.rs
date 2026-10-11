//! ADR-0011 п.2: pipeline position in `metadata_json` and the single derived
//! canonical status. A managed task changes status only via
//! `advance_pipeline`; a task without a position behaves as before.

use std::sync::Arc;

use daruma_core::{Command, CommandHandler};
use daruma_domain::{Actor, NewTask, PipelinePosition, Status, TaskPatch};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::TaskId;
use daruma_storage::{ActivityRepo, CommentRepo, Db, ProjectRepo, SqliteEventStore, TaskRepo};

async fn build_stack() -> (CommandHandler, Arc<TaskRepo>) {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let tasks = Arc::new(TaskRepo::new(pool.clone()));
    let handler = CommandHandler::new(
        store,
        tasks.clone(),
        Arc::new(ProjectRepo::new(pool.clone())),
        Arc::new(CommentRepo::new(pool.clone())),
        Arc::new(ActivityRepo::new(pool)),
        EventBus::default(),
    );
    (handler, tasks)
}

async fn create_task(handler: &CommandHandler) -> TaskId {
    let envs = handler
        .handle(
            Command::CreateTask {
                task: NewTask::new("Pipeline"),
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

async fn advance(handler: &CommandHandler, id: TaskId, position: PipelinePosition) {
    handler
        .handle(
            Command::AdvancePipeline { id, position },
            Actor::agent("builder"),
        )
        .await
        .unwrap();
}

async fn status(tasks: &TaskRepo, id: TaskId) -> Status {
    tasks.get(id).await.unwrap().unwrap().status
}

fn set_status(id: TaskId, status: Status) -> Command {
    Command::SetStatus {
        id,
        status,
        force: false,
        override_reason: None,
        comment: None,
    }
}

fn expect_managed(err: impl std::fmt::Display) {
    let msg = err.to_string();
    assert!(msg.contains("pipeline_managed"), "unexpected error: {msg}");
}

#[tokio::test]
async fn task_without_position_keeps_direct_status_writes() {
    let (handler, tasks) = build_stack().await;
    let id = create_task(&handler).await;
    assert_eq!(tasks.pipeline_position(id).await.unwrap(), None);
    handler
        .handle(set_status(id, Status::InReview), Actor::user())
        .await
        .unwrap();
    handler
        .handle(Command::CompleteTask { id, note: None }, Actor::user())
        .await
        .unwrap();
    assert_eq!(status(&tasks, id).await, Status::Done);
    assert_eq!(tasks.pipeline_position(id).await.unwrap(), None);
}

#[tokio::test]
async fn advance_stores_position_and_derives_status() {
    let (handler, tasks) = build_stack().await;
    let id = create_task(&handler).await;

    let envs = handler
        .handle(
            Command::AdvancePipeline {
                id,
                position: PipelinePosition::at(0, "builder"),
            },
            Actor::agent("builder"),
        )
        .await
        .unwrap();
    assert!(matches!(&envs[0].payload, Event::TaskUpdated { patch, .. }
        if patch.pipeline_position == Some(PipelinePosition::at(0, "builder"))));
    assert!(envs.iter().any(|e| matches!(
        &e.payload,
        Event::TaskStatusChanged {
            to: Status::InProgress,
            ..
        }
    )));
    assert_eq!(
        tasks.pipeline_position(id).await.unwrap(),
        Some(PipelinePosition::at(0, "builder"))
    );
    assert_eq!(status(&tasks, id).await, Status::InProgress);

    advance(&handler, id, PipelinePosition::at(1, "reviewer")).await;
    assert_eq!(status(&tasks, id).await, Status::InReview);

    // Idempotent repeat: same position, no events.
    let again = handler
        .handle(
            Command::AdvancePipeline {
                id,
                position: PipelinePosition::at(1, "reviewer"),
            },
            Actor::agent("reviewer"),
        )
        .await
        .unwrap();
    assert!(again.is_empty());

    advance(&handler, id, PipelinePosition::completed(1, "reviewer")).await;
    assert_eq!(status(&tasks, id).await, Status::Done);
}

#[tokio::test]
async fn managed_task_rejects_every_direct_status_write_but_cancel() {
    let (handler, tasks) = build_stack().await;
    let id = create_task(&handler).await;
    advance(&handler, id, PipelinePosition::at(0, "builder")).await;

    expect_managed(
        handler
            .handle(set_status(id, Status::InReview), Actor::agent("draft"))
            .await
            .unwrap_err(),
    );
    expect_managed(
        handler
            .handle(
                Command::BulkSetStatus {
                    ids: vec![id],
                    status: Status::Done,
                },
                Actor::user(),
            )
            .await
            .unwrap_err(),
    );
    expect_managed(
        handler
            .handle(
                Command::CompleteTask { id, note: None },
                Actor::agent("builder"),
            )
            .await
            .unwrap_err(),
    );
    expect_managed(
        handler
            .handle(
                Command::UpdateTask {
                    id,
                    patch: TaskPatch {
                        status: Some(Status::Done),
                        ..Default::default()
                    },
                },
                Actor::user(),
            )
            .await
            .unwrap_err(),
    );
    // The position itself is never patched directly.
    let err = handler
        .handle(
            Command::UpdateTask {
                id,
                patch: TaskPatch {
                    pipeline_position: Some(PipelinePosition::completed(9, "reviewer")),
                    ..Default::default()
                },
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("advance_pipeline"), "{err}");
    assert_eq!(status(&tasks, id).await, Status::InProgress);
    assert_eq!(
        tasks.pipeline_position(id).await.unwrap(),
        Some(PipelinePosition::at(0, "builder"))
    );

    // Non-status edits still pass, and the owner can cancel.
    handler
        .handle(
            Command::UpdateTask {
                id,
                patch: TaskPatch {
                    description: Some("still editable".into()),
                    ..Default::default()
                },
            },
            Actor::user(),
        )
        .await
        .unwrap();
    assert_eq!(
        tasks.pipeline_position(id).await.unwrap(),
        Some(PipelinePosition::at(0, "builder")),
        "a plain patch keeps metadata_json"
    );
    handler
        .handle(set_status(id, Status::Cancelled), Actor::user())
        .await
        .unwrap();
    assert_eq!(status(&tasks, id).await, Status::Cancelled);
    let err = handler
        .handle(
            Command::AdvancePipeline {
                id,
                position: PipelinePosition::at(1, "reviewer"),
            },
            Actor::agent("reviewer"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("pipeline_cancelled"), "{err}");
}

/// ADR-0011 examples (a)–(d): positions in walk order (as the workflow_policy
/// engine produces them) and the canonical status after each step.
#[tokio::test]
async fn adr_0011_examples_derive_expected_statuses() {
    use PipelinePosition as P;
    use Status::{Done, InProgress as Ip, InReview as Ir};
    let cases: Vec<(&str, Vec<(P, Status)>)> = vec![
        // (a) plan → drafts → builder → reviewer(fail) → builder → reviewer(pass)
        (
            "a",
            vec![
                (P::at(0, "plan"), Ip),
                (P::at(1, "draft"), Ip),
                (P::at(2, "builder"), Ip),
                (P::at(3, "reviewer"), Ir),
                (P::at(2, "builder"), Ip),
                (P::at(3, "reviewer"), Ir),
                (P::completed(3, "reviewer"), Done),
            ],
        ),
        // (b) scout → builder → reviewer, cycle until the reviewer passes
        (
            "b",
            vec![
                (P::at(0, "scout"), Ip),
                (P::at(1, "builder"), Ip),
                (P::at(2, "reviewer"), Ir),
                (P::at(0, "scout"), Ip),
                (P::at(1, "builder"), Ip),
                (P::at(2, "reviewer"), Ir),
                (P::completed(2, "reviewer"), Done),
            ],
        ),
        // (c) builder ×3, no review stage at all
        (
            "c",
            vec![
                (P::at(0, "builder"), Ip),
                (P::at(1, "builder"), Ip),
                (P::at(2, "builder"), Ip),
                (P::completed(2, "builder"), Done),
            ],
        ),
        // (d) one review over the whole diff
        (
            "d",
            vec![
                (P::at(0, "plan"), Ip),
                (P::at(1, "scout"), Ip),
                (P::at(2, "builder"), Ip),
                (P::at(3, "builder"), Ip),
                (P::at(4, "builder"), Ip),
                (P::at(5, "reviewer"), Ir),
                (P::completed(5, "reviewer"), Done),
            ],
        ),
    ];
    let (handler, tasks) = build_stack().await;
    for (name, steps) in cases {
        let id = create_task(&handler).await;
        for (position, expected) in steps {
            advance(&handler, id, position.clone()).await;
            assert_eq!(
                status(&tasks, id).await,
                expected,
                "example ({name}) at {position:?}"
            );
            assert_eq!(tasks.pipeline_position(id).await.unwrap(), Some(position));
        }
    }
}
