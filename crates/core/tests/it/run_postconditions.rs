//! Pass completion contract, S3 (docs/mcp/EXECUTOR-LOOP.md §«Pass completion
//! contract», postconditions 1–3): `CompleteRun` is rejected while a step is
//! open or a step finished `done` has no handoff transition of its task after
//! the step started. Runs without steps and steps finished with any other
//! outcome complete as before.
//!
//! The check is deterministic (events + projections only) and needs no AI
//! provider, so there is one behaviour for both operating modes.

use std::sync::Arc;

use daruma_core::{Command, CommandHandler};
use daruma_domain::{Actor, NewPlan, NewTask, PipelinePosition, PlanStatus, RunOutcome, Status};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::{AgentId, PlanId, ProjectId, RunId, TaskId};
use daruma_storage::{
    ActivityRepo, CommentRepo, Db, PlanRepo, ProjectRepo, RunRepo, SqliteEventStore, TaskRepo,
};

fn handler_on(db: &Db) -> CommandHandler {
    let pool = db.pool();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    CommandHandler::new(
        store,
        Arc::new(TaskRepo::new(pool.clone())),
        Arc::new(ProjectRepo::new(pool.clone())),
        Arc::new(CommentRepo::new(pool.clone())),
        Arc::new(ActivityRepo::new(pool.clone())),
        EventBus::default(),
    )
    .with_plans(Arc::new(PlanRepo::new(pool.clone())))
    .with_runs(Arc::new(RunRepo::new(pool.clone())))
}

async fn db() -> Db {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    db
}

async fn active_plan(handler: &CommandHandler) -> PlanId {
    let envs = handler
        .handle(
            Command::CreatePlan {
                plan: NewPlan::new("Postconditions", ProjectId::new(), Actor::user()),
                external_ref: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let Event::PlanCreated { plan } = &envs[0].payload else {
        panic!("expected PlanCreated")
    };
    handler
        .handle(
            Command::SetPlanStatus {
                plan_id: plan.id,
                status: PlanStatus::Active,
                force: false,
                override_reason: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    plan.id
}

async fn in_progress_task(handler: &CommandHandler) -> TaskId {
    let envs = handler
        .handle(
            Command::CreateTask {
                task: NewTask::new("Builder pass"),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let Event::TaskCreated { task } = &envs[0].payload else {
        panic!("expected TaskCreated")
    };
    let id = task.id.unwrap();
    set_status(handler, id, Status::InProgress).await;
    id
}

async fn start_run(handler: &CommandHandler) -> RunId {
    let plan_id = active_plan(handler).await;
    let envs = handler
        .handle(
            Command::StartRun {
                plan_id,
                agent_id: AgentId::new(),
                parent_run_id: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let Event::RunStarted { run } = &envs[0].payload else {
        panic!("expected RunStarted")
    };
    run.id
}

async fn set_status(handler: &CommandHandler, id: TaskId, status: Status) {
    handler
        .handle(
            Command::SetStatus {
                id,
                status,
                force: false,
                override_reason: None,
                comment: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
}

async fn start_step(handler: &CommandHandler, run_id: RunId, task_id: TaskId) {
    handler
        .handle(Command::RunStartStep { run_id, task_id }, Actor::user())
        .await
        .unwrap();
}

async fn finish_step(
    handler: &CommandHandler,
    run_id: RunId,
    task_id: TaskId,
    outcome: RunOutcome,
) {
    handler
        .handle(
            Command::RunFinishStep {
                run_id,
                task_id,
                outcome,
            },
            Actor::user(),
        )
        .await
        .unwrap();
}

async fn complete(handler: &CommandHandler, run_id: RunId) -> daruma_shared::Result<()> {
    handler
        .handle(Command::CompleteRun { run_id }, Actor::user())
        .await
        .map(|envs| {
            assert!(matches!(envs[0].payload, Event::RunCompleted { .. }));
        })
}

/// The rejection is a conflict naming the run and the task, and no
/// `RunCompleted` lands: the run stays active.
async fn assert_rejected(db: &Db, handler: &CommandHandler, run_id: RunId, task_id: TaskId) {
    let err = complete(handler, run_id)
        .await
        .expect_err("must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("run_postcondition_unmet"), "{msg}");
    assert!(msg.contains(&run_id.to_string()), "{msg}");
    assert!(msg.contains(&task_id.to_string()), "{msg}");
    let run = RunRepo::new(db.pool().clone())
        .get(run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, daruma_domain::RunStatus::Active);
}

// Reproduces 01a126e4: the builder's set_status failed (wrong parameter), so
// the task never left in_progress, yet the pass finished its step `done` and
// called run_complete.
#[tokio::test]
async fn done_step_without_handoff_rejects_complete() {
    let db = db().await;
    let handler = handler_on(&db);
    let task_id = in_progress_task(&handler).await;
    let run_id = start_run(&handler).await;
    start_step(&handler, run_id, task_id).await;
    let failed = handler
        .handle(
            Command::SetStatus {
                id: TaskId::new(),
                status: Status::InReview,
                force: false,
                override_reason: None,
                comment: None,
            },
            Actor::user(),
        )
        .await;
    assert!(failed.is_err(), "the mistyped set_status must fail");
    finish_step(&handler, run_id, task_id, RunOutcome::Done).await;

    assert_rejected(&db, &handler, run_id, task_id).await;
}

#[tokio::test]
async fn builder_handoff_completes_and_survives_later_reject() {
    let db = db().await;
    let handler = handler_on(&db);
    let task_id = in_progress_task(&handler).await;
    let run_id = start_run(&handler).await;
    start_step(&handler, run_id, task_id).await;
    finish_step(&handler, run_id, task_id, RunOutcome::Done).await;
    set_status(&handler, task_id, Status::InReview).await;
    // A legitimate REVIEW REJECT and a new claim land before the (retried)
    // completion: the earlier handoff still proves the attempt.
    set_status(&handler, task_id, Status::Todo).await;
    set_status(&handler, task_id, Status::InProgress).await;

    complete(&handler, run_id).await.unwrap();
}

#[tokio::test]
async fn open_step_rejects_complete() {
    let db = db().await;
    let handler = handler_on(&db);
    let task_id = in_progress_task(&handler).await;
    let run_id = start_run(&handler).await;
    start_step(&handler, run_id, task_id).await;
    set_status(&handler, task_id, Status::InReview).await;

    assert_rejected(&db, &handler, run_id, task_id).await;
}

#[tokio::test]
async fn handoff_before_step_start_does_not_count() {
    let db = db().await;
    let handler = handler_on(&db);
    let task_id = in_progress_task(&handler).await;
    set_status(&handler, task_id, Status::InReview).await;
    let run_id = start_run(&handler).await;
    start_step(&handler, run_id, task_id).await;
    finish_step(&handler, run_id, task_id, RunOutcome::Done).await;

    assert_rejected(&db, &handler, run_id, task_id).await;
}

// The executor loop of docs/mcp/EXECUTOR-LOOP.md completes the task before
// `run_finish_step`. Without `run_start_step` the finish opens the attempt at
// its own timestamp, after the completion, so the run is rejected; with the
// step started before the work the same order completes (01a144fe…9c66985bf9a4).
#[tokio::test]
async fn executor_loop_order_needs_run_start_step() {
    let db = db().await;
    let handler = handler_on(&db);

    let skipped = in_progress_task(&handler).await;
    let run_skipped = start_run(&handler).await;
    handler
        .handle(
            Command::CompleteTask {
                id: skipped,
                note: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    finish_step(&handler, run_skipped, skipped, RunOutcome::Done).await;
    assert_rejected(&db, &handler, run_skipped, skipped).await;

    let started = in_progress_task(&handler).await;
    let run_started = start_run(&handler).await;
    start_step(&handler, run_started, started).await;
    handler
        .handle(
            Command::CompleteTask {
                id: started,
                note: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    finish_step(&handler, run_started, started, RunOutcome::Done).await;
    complete(&handler, run_started).await.unwrap();
}

#[tokio::test]
async fn reviewer_accept_and_reject_are_handoffs() {
    let db = db().await;
    let handler = handler_on(&db);
    let accepted = in_progress_task(&handler).await;
    let rejected = in_progress_task(&handler).await;
    set_status(&handler, accepted, Status::InReview).await;
    set_status(&handler, rejected, Status::InReview).await;
    let run_id = start_run(&handler).await;
    start_step(&handler, run_id, accepted).await;
    set_status(&handler, accepted, Status::Done).await;
    finish_step(&handler, run_id, accepted, RunOutcome::Done).await;
    start_step(&handler, run_id, rejected).await;
    set_status(&handler, rejected, Status::Todo).await;
    finish_step(&handler, run_id, rejected, RunOutcome::Done).await;

    complete(&handler, run_id).await.unwrap();
}

#[tokio::test]
async fn pipeline_advance_is_the_handoff_of_a_managed_task() {
    let db = db().await;
    let handler = handler_on(&db);
    let task_id = in_progress_task(&handler).await;
    let advance = |position| Command::AdvancePipeline {
        id: task_id,
        position,
    };
    handler
        .handle(advance(PipelinePosition::at(0, "draft")), Actor::user())
        .await
        .unwrap();
    let run_id = start_run(&handler).await;
    start_step(&handler, run_id, task_id).await;
    // draft → builder keeps the canonical status in_progress: only the
    // position change records the handoff.
    handler
        .handle(advance(PipelinePosition::at(1, "builder")), Actor::user())
        .await
        .unwrap();
    finish_step(&handler, run_id, task_id, RunOutcome::Done).await;

    complete(&handler, run_id).await.unwrap();
}

#[tokio::test]
async fn runs_without_steps_or_with_failed_steps_stay_compatible() {
    let db = db().await;
    let handler = handler_on(&db);
    let research = start_run(&handler).await;
    complete(&handler, research).await.unwrap();

    let task_id = in_progress_task(&handler).await;
    let run_id = start_run(&handler).await;
    start_step(&handler, run_id, task_id).await;
    finish_step(
        &handler,
        run_id,
        task_id,
        RunOutcome::Failed {
            reason: "error[E0308]: mismatched types".into(),
        },
    )
    .await;
    complete(&handler, run_id).await.unwrap();
}

// Hosted dispatch may serve one workspace from more than one handler instance
// (tenant cache invalidation, cabinet bus). The check reads the shared
// projections, so a step opened through one instance blocks completion
// through another, and a transition recorded through one instance satisfies
// completion through another — no reliance on either instance's mutex.
#[tokio::test]
async fn postconditions_hold_across_handler_instances() {
    let db = db().await;
    let writer = handler_on(&db);
    let completer = handler_on(&db);
    let task_id = in_progress_task(&writer).await;
    let run_id = start_run(&writer).await;
    start_step(&writer, run_id, task_id).await;

    assert_rejected(&db, &completer, run_id, task_id).await;

    finish_step(&writer, run_id, task_id, RunOutcome::Done).await;
    assert_rejected(&db, &completer, run_id, task_id).await;

    set_status(&writer, task_id, Status::InReview).await;
    complete(&completer, run_id).await.unwrap();
}
