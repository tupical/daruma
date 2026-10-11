//! ADR-0011 п.4: fan-in barrier for parallel stages. Each parallel instance
//! reports only its own part (`close_pipeline_stage`); the stage — and the
//! canonical status derived from it — advances only when every required
//! instance closed, exactly once.

use std::sync::Arc;

use daruma_core::{Command, CommandHandler};
use daruma_domain::{Actor, NewTask, PipelinePosition, Status};
use daruma_events::{Event, EventBus, EventEnvelope, EventStore};
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
                task: NewTask::new("Fan-in"),
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

async fn advance(
    handler: &CommandHandler,
    id: TaskId,
    position: PipelinePosition,
) -> Result<Vec<EventEnvelope>, String> {
    handler
        .handle(
            Command::AdvancePipeline { id, position },
            Actor::agent("orchestrator"),
        )
        .await
        .map_err(|e| e.to_string())
}

async fn close(
    handler: &CommandHandler,
    id: TaskId,
    stage_index: u32,
    instance: &str,
    then: Option<PipelinePosition>,
) -> Result<Vec<EventEnvelope>, String> {
    handler
        .handle(
            Command::ClosePipelineStage {
                id,
                stage_index,
                instance: instance.into(),
                then,
            },
            Actor::agent(instance),
        )
        .await
        .map_err(|e| e.to_string())
}

async fn status(tasks: &TaskRepo, id: TaskId) -> Status {
    tasks.get(id).await.unwrap().unwrap().status
}

async fn position(tasks: &TaskRepo, id: TaskId) -> PipelinePosition {
    tasks.pipeline_position(id).await.unwrap().unwrap()
}

fn status_changes(envs: &[EventEnvelope]) -> usize {
    envs.iter()
        .filter(|e| matches!(e.payload, Event::TaskStatusChanged { .. }))
        .count()
}

fn drafts(n: u32) -> PipelinePosition {
    PipelinePosition::at(1, "draft").fanout(n)
}

fn builder() -> PipelinePosition {
    PipelinePosition::at(2, "builder")
}

/// Task at plan(0), then at the draft stage with `n` parallel instances.
async fn at_drafts(handler: &CommandHandler, n: u32) -> TaskId {
    let id = create_task(handler).await;
    advance(handler, id, PipelinePosition::at(0, "plan"))
        .await
        .unwrap();
    advance(handler, id, drafts(n)).await.unwrap();
    id
}

#[tokio::test]
async fn n_minus_one_of_n_does_not_advance() {
    let (handler, tasks) = build_stack().await;
    let id = at_drafts(&handler, 3).await;

    for d in ["draft-1", "draft-2"] {
        let envs = close(&handler, id, 1, d, Some(builder())).await.unwrap();
        assert_eq!(status_changes(&envs), 0, "{d} must not touch the status");
    }
    let pos = position(&tasks, id).await;
    assert_eq!((pos.stage_index, pos.closed.len()), (1, 2));

    // The centralized advance refuses to leave the stage at 2 of 3.
    let err = advance(&handler, id, builder()).await.unwrap_err();
    assert!(err.contains("fan_in_pending"), "{err}");
    assert!(err.contains("closed 2 of 3"), "{err}");
    assert_eq!(position(&tasks, id).await.stage_index, 1);
    assert_eq!(status(&tasks, id).await, Status::InProgress);
}

#[tokio::test]
async fn closing_the_barrier_advances_exactly_once() {
    let (handler, tasks) = build_stack().await;
    let id = at_drafts(&handler, 3).await;
    close(&handler, id, 1, "draft-1", Some(builder()))
        .await
        .unwrap();
    // A repeated report is a no-op, not a second closed instance.
    assert!(close(&handler, id, 1, "draft-1", Some(builder()))
        .await
        .unwrap()
        .is_empty());
    close(&handler, id, 1, "draft-2", Some(builder()))
        .await
        .unwrap();

    // The last two instances race: one closes the barrier and advances, the
    // other lands on the moved stage and is refused without a second advance.
    let (a, b) = tokio::join!(
        close(&handler, id, 1, "draft-3", Some(builder())),
        close(&handler, id, 1, "draft-4", Some(builder())),
    );
    let (won, lost) = match (a, b) {
        (Ok(w), Err(l)) | (Err(l), Ok(w)) => (w, l),
        other => panic!("expected one winner, got {other:?}"),
    };
    assert!(
        lost.contains("stale_stage") || lost.contains("fan_in_full"),
        "{lost}"
    );
    let advanced: Vec<_> = won
        .iter()
        .filter(|e| {
            matches!(&e.payload, Event::TaskUpdated { patch, .. }
            if patch.pipeline_position == Some(builder()))
        })
        .collect();
    assert_eq!(advanced.len(), 1);
    assert_eq!(position(&tasks, id).await, builder());

    // Retrying the closing report or the advance changes nothing.
    let retry = close(&handler, id, 1, "draft-3", Some(builder()))
        .await
        .unwrap_err();
    assert!(retry.contains("stale_stage"), "{retry}");
    assert!(advance(&handler, id, builder()).await.unwrap().is_empty());
    assert_eq!(position(&tasks, id).await, builder());
}

#[tokio::test]
async fn racing_orchestrators_advance_once_after_barrier() {
    let (handler, tasks) = build_stack().await;
    let id = at_drafts(&handler, 2).await;
    close(&handler, id, 1, "draft-1", None).await.unwrap();
    // Without `then` the closing report only closes the barrier.
    let envs = close(&handler, id, 1, "draft-2", None).await.unwrap();
    assert_eq!(status_changes(&envs), 0);
    assert_eq!(position(&tasks, id).await.stage_index, 1);

    let (a, b) = tokio::join!(
        advance(&handler, id, builder()),
        advance(&handler, id, builder())
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(
        [a.is_empty(), b.is_empty()].iter().filter(|e| **e).count(),
        1,
        "exactly one advance emits events"
    );
    assert_eq!(position(&tasks, id).await, builder());
}

#[tokio::test]
async fn parallel_instance_cannot_write_status_directly() {
    let (handler, tasks) = build_stack().await;
    let id = at_drafts(&handler, 2).await;
    for cmd in [
        Command::SetStatus {
            id,
            status: Status::InReview,
            force: false,
            override_reason: None,
            comment: None,
        },
        Command::CompleteTask { id, note: None },
    ] {
        let err = handler
            .handle(cmd, Actor::agent("draft-2"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("pipeline_managed"), "{err}");
    }
    // Nor can an instance jump its own stage forward before the fan-in.
    let err = handler
        .handle(
            Command::AdvancePipeline {
                id,
                position: builder(),
            },
            Actor::agent("draft-2"),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("fan_in_pending"), "{err}");
    // Nor smuggle closed instances into a position.
    let mut forged = drafts(2);
    forged.closed = vec!["draft-1".into(), "draft-2".into()];
    let err = handler
        .handle(
            Command::AdvancePipeline {
                id,
                position: forged,
            },
            Actor::agent("draft-2"),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("closed is filled only"), "{err}");
    assert_eq!(status(&tasks, id).await, Status::InProgress);
}

#[tokio::test]
async fn close_is_refused_off_pipeline_or_on_other_stage() {
    let (handler, _tasks) = build_stack().await;
    let unmanaged = create_task(&handler).await;
    let err = close(&handler, unmanaged, 0, "d", None).await.unwrap_err();
    assert!(err.contains("pipeline_unmanaged"), "{err}");

    let id = at_drafts(&handler, 2).await;
    let err = close(&handler, id, 0, "d", None).await.unwrap_err();
    assert!(err.contains("stale_stage"), "{err}");
}

#[tokio::test]
async fn optional_draft_stage_may_be_skipped() {
    let (handler, tasks) = build_stack().await;
    let id = create_task(&handler).await;
    advance(&handler, id, drafts(3).optional()).await.unwrap();
    close(&handler, id, 1, "draft-1", None).await.unwrap();
    advance(&handler, id, builder()).await.unwrap();
    assert_eq!(position(&tasks, id).await, builder());
}

/// ADR-0011 example (a): plan → drafts (parallel) → builder → reviewer →
/// builder → reviewer, with the reviewer passing on the second round.
#[tokio::test]
async fn example_a_end_to_end() {
    let (handler, tasks) = build_stack().await;
    let reviewer = PipelinePosition::at(3, "reviewer");
    let id = create_task(&handler).await;
    let mut trace = vec![];

    advance(&handler, id, PipelinePosition::at(0, "plan"))
        .await
        .unwrap();
    trace.push(status(&tasks, id).await);
    advance(&handler, id, drafts(3)).await.unwrap();
    for d in ["draft-1", "draft-2", "draft-3"] {
        close(&handler, id, 1, d, Some(builder())).await.unwrap();
        trace.push(status(&tasks, id).await);
    }
    assert_eq!(position(&tasks, id).await, builder());
    advance(&handler, id, reviewer.clone()).await.unwrap();
    trace.push(status(&tasks, id).await);
    advance(&handler, id, builder()).await.unwrap(); // review failed: back to builder
    trace.push(status(&tasks, id).await);
    advance(&handler, id, reviewer.clone()).await.unwrap();
    trace.push(status(&tasks, id).await);
    advance(&handler, id, PipelinePosition::completed(3, "reviewer"))
        .await
        .unwrap();
    trace.push(status(&tasks, id).await);

    use Status::*;
    assert_eq!(
        trace,
        [InProgress, InProgress, InProgress, InProgress, InReview, InProgress, InReview, Done]
    );
}
