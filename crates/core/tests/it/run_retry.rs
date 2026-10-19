//! Pass completion contract, S4 (docs/mcp/EXECUTOR-LOOP.md §«Pass completion
//! contract», postcondition 4): a launcher that lost the response of
//! `run_complete` or `run_finish_step` may repeat the call. The owner's repeat
//! of `CompleteRun` on a completed run is a no-op success without a second
//! `RunCompleted`; another agent, an unauthenticated caller, or a
//! failed/aborted run still get the `run is not active` conflict. A repeated
//! `RunFinishStep` with the outcome the latest attempt already holds records
//! no synthetic attempt.
//!
//! The check is deterministic (projections only) and needs no AI provider, so
//! there is one behaviour for both operating modes.

use std::sync::Arc;

use daruma_core::{Command, CommandHandler};
use daruma_domain::{Actor, NewPlan, NewTask, PlanStatus, RunOutcome, RunStatus, Status};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::{AgentId, PlanId, ProjectId, RunId, TaskId};
use daruma_storage::{
    ActivityRepo, AgentClaimRepo, CommentRepo, Db, PlanRepo, ProjectRepo, RunRepo,
    SqliteEventStore, TaskRepo,
};

struct Stack {
    handler: CommandHandler,
    store: Arc<dyn EventStore>,
    runs: RunRepo,
}

async fn stack() -> Stack {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let handler = CommandHandler::new(
        store.clone(),
        Arc::new(TaskRepo::new(pool.clone())),
        Arc::new(ProjectRepo::new(pool.clone())),
        Arc::new(CommentRepo::new(pool.clone())),
        Arc::new(ActivityRepo::new(pool.clone())),
        EventBus::default(),
    )
    .with_plans(Arc::new(PlanRepo::new(pool.clone())))
    .with_runs(Arc::new(RunRepo::new(pool.clone())))
    .with_claims(Arc::new(AgentClaimRepo::new(pool.clone())));
    Stack {
        handler,
        store,
        runs: RunRepo::new(pool),
    }
}

async fn active_plan(handler: &CommandHandler) -> PlanId {
    let envs = handler
        .handle(
            Command::CreatePlan {
                plan: NewPlan::new("Retry", ProjectId::new(), Actor::user()),
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

/// Start a run through the owned path, as `/v1/runs` and drain-next do.
async fn owned_run(handler: &CommandHandler, owner: AgentId) -> RunId {
    let plan_id = active_plan(handler).await;
    let out = handler
        .handle_authenticated_with_warnings(
            Command::StartRun {
                plan_id,
                agent_id: owner,
                parent_run_id: None,
            },
            Actor::user(),
            owner,
            false,
        )
        .await
        .unwrap();
    let Event::RunStarted { run } = &out.events[0].payload else {
        panic!("expected RunStarted")
    };
    run.id
}

async fn complete_as(
    handler: &CommandHandler,
    run_id: RunId,
    caller: AgentId,
) -> daruma_shared::Result<usize> {
    complete_with(handler, run_id, caller, false).await
}

async fn complete_with(
    handler: &CommandHandler,
    run_id: RunId,
    caller: AgentId,
    is_admin: bool,
) -> daruma_shared::Result<usize> {
    handler
        .handle_authenticated_with_warnings(
            Command::CompleteRun { run_id },
            Actor::user(),
            caller,
            is_admin,
        )
        .await
        .map(|out| out.events.len())
}

async fn count(store: &Arc<dyn EventStore>, pick: fn(&Event) -> bool) -> usize {
    store
        .load_since(0, 10_000)
        .await
        .unwrap()
        .iter()
        .filter(|env| pick(&env.payload))
        .count()
}

fn run_completed(event: &Event) -> bool {
    matches!(event, Event::RunCompleted { .. })
}

fn step_finished(event: &Event) -> bool {
    matches!(event, Event::RunStepFinished { .. })
}

// Lost response: the first call completed the run, the retry by the same
// owner succeeds without appending a second RunCompleted.
#[tokio::test]
async fn owner_retry_of_completed_run_is_noop() {
    let s = stack().await;
    let owner = AgentId::new();
    let run_id = owned_run(&s.handler, owner).await;

    assert_eq!(complete_as(&s.handler, run_id, owner).await.unwrap(), 1);
    assert_eq!(complete_as(&s.handler, run_id, owner).await.unwrap(), 0);
    assert_eq!(count(&s.store, run_completed).await, 1);
    let run = s.runs.get(run_id).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Completed);
}

// The no-op is the owner's only: another agent and an unauthenticated
// caller still get the conflict, and nothing is appended.
#[tokio::test]
async fn foreign_retry_of_completed_run_stays_conflict() {
    let s = stack().await;
    let owner = AgentId::new();
    let run_id = owned_run(&s.handler, owner).await;
    complete_as(&s.handler, run_id, owner).await.unwrap();

    let err = complete_as(&s.handler, run_id, AgentId::new())
        .await
        .expect_err("foreign retry must be refused");
    assert!(err.to_string().contains("run is not active"), "{err}");
    let err = s
        .handler
        .handle(Command::CompleteRun { run_id }, Actor::user())
        .await
        .expect_err("unauthenticated retry must be refused");
    assert!(err.to_string().contains("run is not active"), "{err}");
    assert_eq!(count(&s.store, run_completed).await, 1);
}

// docs/PLAN_RECONCILIATION.md: run ownership is not bypassed. Another
// `RunWrite` holder cannot complete an active owned run — the open run keeps
// its plan open — while an admin can, like fail/abort cleanup.
#[tokio::test]
async fn foreign_complete_of_active_owned_run_is_forbidden() {
    let s = stack().await;
    let owner = AgentId::new();
    let run_id = owned_run(&s.handler, owner).await;

    let err = complete_as(&s.handler, run_id, AgentId::new())
        .await
        .expect_err("foreign completion must be refused");
    assert_eq!(err.code(), "forbidden", "{err}");
    assert!(err.to_string().contains("owned by another agent"), "{err}");
    let run = s.runs.get(run_id).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Active);
    assert_eq!(count(&s.store, run_completed).await, 0);

    assert_eq!(complete_as(&s.handler, run_id, owner).await.unwrap(), 1);
    assert_eq!(count(&s.store, run_completed).await, 1);
}

#[tokio::test]
async fn admin_may_complete_foreign_owned_run() {
    let s = stack().await;
    let run_id = owned_run(&s.handler, AgentId::new()).await;

    assert_eq!(
        complete_with(&s.handler, run_id, AgentId::new(), true)
            .await
            .unwrap(),
        1
    );
    let run = s.runs.get(run_id).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Completed);
}

// A run started on the unauthenticated path has no recorded owner, so there
// is no ownership to enforce: an authenticated caller still completes it.
#[tokio::test]
async fn unowned_run_stays_completable_by_authenticated_caller() {
    let s = stack().await;
    let plan_id = active_plan(&s.handler).await;
    let envs = s
        .handler
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

    assert_eq!(
        complete_as(&s.handler, run.id, AgentId::new())
            .await
            .unwrap(),
        1
    );
    assert_eq!(count(&s.store, run_completed).await, 1);
}

// Failed and aborted runs never turn into completed ones, even for the owner.
#[tokio::test]
async fn terminal_failed_or_aborted_run_stays_conflict() {
    let s = stack().await;
    let owner = AgentId::new();
    for abort in [true, false] {
        let run_id = owned_run(&s.handler, owner).await;
        let cmd = if abort {
            Command::AbortRun {
                run_id,
                reason: "blocked".into(),
            }
        } else {
            Command::FailRun {
                run_id,
                reason: "failed".into(),
            }
        };
        s.handler
            .handle_authenticated_with_warnings(cmd, Actor::user(), owner, false)
            .await
            .unwrap();

        let err = complete_as(&s.handler, run_id, owner)
            .await
            .expect_err("terminal run must stay terminal");
        assert!(err.to_string().contains("run is not active"), "{err}");
        let status = s.runs.get(run_id).await.unwrap().unwrap().status;
        let expected = if abort {
            RunStatus::Aborted
        } else {
            RunStatus::Failed
        };
        assert_eq!(status, expected);
    }
    assert_eq!(count(&s.store, run_completed).await, 0);
}

// A repeated `run_finish_step{done}` for the already closed last attempt is a
// no-op: one step row, one RunStepFinished; and the run still completes once
// the handoff transition exists.
#[tokio::test]
async fn repeated_finish_step_records_no_synthetic_attempt() {
    let s = stack().await;
    let owner = AgentId::new();
    let run_id = owned_run(&s.handler, owner).await;
    let envs = s
        .handler
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
    let task_id: TaskId = task.id.unwrap();
    set_status(&s.handler, task_id, Status::InProgress).await;
    s.handler
        .handle(Command::RunStartStep { run_id, task_id }, Actor::user())
        .await
        .unwrap();
    set_status(&s.handler, task_id, Status::InReview).await;

    let finish = |outcome: RunOutcome| Command::RunFinishStep {
        run_id,
        task_id,
        outcome,
    };
    let first = s
        .handler
        .handle(finish(RunOutcome::Done), Actor::user())
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    let retry = s
        .handler
        .handle(finish(RunOutcome::Done), Actor::user())
        .await
        .unwrap();
    assert!(retry.is_empty(), "retry must append nothing: {retry:?}");
    assert_eq!(s.runs.list_steps(run_id).await.unwrap().len(), 1);
    assert_eq!(count(&s.store, step_finished).await, 1);

    assert_eq!(complete_as(&s.handler, run_id, owner).await.unwrap(), 1);
    assert_eq!(complete_as(&s.handler, run_id, owner).await.unwrap(), 0);
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
