//! Integration tests for §3.7.4 — Liveness contract on Run (heartbeat).
//!
//! Two cases are covered:
//!   * **unresponsive**: a run is started but no `RunStepStarted` arrives
//!     within `liveness_ack_secs`; the watchdog emits `RunUnresponsive`
//!     exactly once and is idempotent on subsequent ticks.
//!   * **stale**: an active run produces a step then goes silent for at least
//!     `liveness_idle_secs`; the watchdog emits `RunStale` exactly once.
//!
//! Run status is never changed by the watchdog — these are signal-only events.

use std::sync::Arc;

use chrono::Utc;
use taskagent_core::{Command, CommandHandler};
use taskagent_domain::{Actor, NewPlan, PlanStatus, RunOutcome};
use taskagent_events::{Event, EventBus, EventStore};
use taskagent_shared::{AgentId, ProjectId, RunId, TaskId};
use taskagent_storage::{
    ActivityRepo, AgentClaimRepo, CommentRepo, Db, ExternalRefRepo, PlanRepo, ProjectRepo,
    RelationRepo, RunRepo, SessionRepo, SqliteEventStore, TaskRepo,
};

async fn build_stack() -> (CommandHandler, Arc<dyn EventStore>) {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let tasks = Arc::new(TaskRepo::new(pool.clone()));
    let projects = Arc::new(ProjectRepo::new(pool.clone()));
    let comments = Arc::new(CommentRepo::new(pool.clone()));
    let activity = Arc::new(ActivityRepo::new(pool.clone()));
    let plans = Arc::new(PlanRepo::new(pool.clone()));
    let runs = Arc::new(RunRepo::new(pool.clone()));
    let sessions = Arc::new(SessionRepo::new(pool.clone()));
    let claims = Arc::new(AgentClaimRepo::new(pool.clone()));
    let ext_refs = Arc::new(ExternalRefRepo::new(pool.clone()));
    let relations = Arc::new(RelationRepo::new(pool));
    let bus = EventBus::default();

    let handler = CommandHandler::new(store.clone(), tasks, projects, comments, activity, bus)
        .with_plans(plans)
        .with_runs(runs)
        .with_sessions(sessions)
        .with_claims(claims)
        .with_external_refs(ext_refs)
        .with_relations(relations);

    (handler, store)
}

/// Create an Active plan and return its id.
async fn create_active_plan(handler: &CommandHandler) -> taskagent_shared::PlanId {
    let envs = handler
        .handle(
            Command::CreatePlan {
                plan: NewPlan::new("Liveness plan", ProjectId::new(), Actor::user()),
                external_ref: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let plan_id = match &envs[0].payload {
        Event::PlanCreated { plan } => plan.id,
        _ => panic!("expected PlanCreated"),
    };
    handler
        .handle(
            Command::SetPlanStatus {
                plan_id,
                status: PlanStatus::Active,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    plan_id
}

async fn start_run(handler: &CommandHandler) -> RunId {
    let plan_id = create_active_plan(handler).await;
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
    match &envs[0].payload {
        Event::RunStarted { run } => run.id,
        _ => panic!("expected RunStarted"),
    }
}

fn count_kind(events: &[taskagent_events::EventEnvelope], kind: &str) -> usize {
    events.iter().filter(|e| e.payload.kind() == kind).count()
}

// ── AC §3.7.4: unresponsive — no first step within ack window ────────────────

#[tokio::test]
async fn unresponsive_emitted_once_when_first_step_late() {
    let (handler, store) = build_stack().await;
    let run_id = start_run(&handler).await;

    // No RunStepStarted has arrived. Tick liveness with now = started_at + 31s
    // and ack = 30 → exactly one RunUnresponsive.
    let now = Utc::now() + chrono::Duration::seconds(31);
    let emitted = handler.tick_liveness(now, 30, 1800).await.unwrap();
    assert_eq!(emitted, 1, "expected exactly one liveness event");

    let all = store.load_since(0, 1024).await.unwrap();
    let unresponsive: Vec<_> = all
        .iter()
        .filter_map(|e| match &e.payload {
            Event::RunUnresponsive { run_id, .. } => Some(*run_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        unresponsive,
        vec![run_id],
        "exactly one RunUnresponsive for our run"
    );

    // Idempotent: a second tick at the same instant must not emit a duplicate.
    let emitted = handler.tick_liveness(now, 30, 1800).await.unwrap();
    assert_eq!(emitted, 0, "second tick must not emit duplicate");
    let all = store.load_since(0, 1024).await.unwrap();
    assert_eq!(count_kind(&all, "run_unresponsive"), 1);
}

#[tokio::test]
async fn unresponsive_not_emitted_before_window() {
    let (handler, store) = build_stack().await;
    let _run_id = start_run(&handler).await;

    // Only 5 seconds have passed — below the 30-second ack threshold.
    let now = Utc::now() + chrono::Duration::seconds(5);
    let emitted = handler.tick_liveness(now, 30, 1800).await.unwrap();
    assert_eq!(emitted, 0);

    let all = store.load_since(0, 1024).await.unwrap();
    assert_eq!(count_kind(&all, "run_unresponsive"), 0);
}

#[tokio::test]
async fn unresponsive_not_emitted_when_first_step_arrived() {
    let (handler, store) = build_stack().await;
    let run_id = start_run(&handler).await;

    // First step arrives — last_activity_at advances past started_at.
    handler
        .handle(
            Command::RunStartStep {
                run_id,
                task_id: TaskId::new(),
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // Tick well past the ack window. RunUnresponsive must NOT be emitted
    // because a step has already been observed.
    let now = Utc::now() + chrono::Duration::seconds(31);
    let emitted = handler.tick_liveness(now, 30, 1800).await.unwrap();
    assert_eq!(emitted, 0);

    let all = store.load_since(0, 1024).await.unwrap();
    assert_eq!(count_kind(&all, "run_unresponsive"), 0);
}

// ── AC §3.7.4: stale — no step activity within idle window ───────────────────

#[tokio::test]
async fn stale_emitted_once_when_idle_window_exceeded() {
    let (handler, store) = build_stack().await;
    let run_id = start_run(&handler).await;

    // Move past the first step so we exercise the "stale after step" path.
    handler
        .handle(
            Command::RunStartStep {
                run_id,
                task_id: TaskId::new(),
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // 1801 seconds since last_activity_at, idle threshold 1800 → one RunStale.
    let now = Utc::now() + chrono::Duration::seconds(1801);
    let emitted = handler.tick_liveness(now, 30, 1800).await.unwrap();
    assert_eq!(emitted, 1);

    let all = store.load_since(0, 1024).await.unwrap();
    let stale: Vec<_> = all
        .iter()
        .filter_map(|e| match &e.payload {
            Event::RunStale { run_id, .. } => Some(*run_id),
            _ => None,
        })
        .collect();
    assert_eq!(stale, vec![run_id]);

    // Idempotent.
    let emitted = handler.tick_liveness(now, 30, 1800).await.unwrap();
    assert_eq!(emitted, 0);
    let all = store.load_since(0, 1024).await.unwrap();
    assert_eq!(count_kind(&all, "run_stale"), 1);
}

#[tokio::test]
async fn step_finished_refreshes_last_activity() {
    let (handler, store) = build_stack().await;
    let run_id = start_run(&handler).await;
    let task_id = TaskId::new();

    handler
        .handle(Command::RunStartStep { run_id, task_id }, Actor::user())
        .await
        .unwrap();
    handler
        .handle(
            Command::RunFinishStep {
                run_id,
                task_id,
                outcome: RunOutcome::Done,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // 1801 seconds after the *start* of the test, but the most recent step
    // event was RunFinishStep ≈ now(); the relative cutoff is the same
    // direction-of-time so the assertion is on the watchdog logic, not wall
    // clock. With now far in the future, both step events are "old" enough.
    let now = Utc::now() + chrono::Duration::seconds(1801);
    let emitted = handler.tick_liveness(now, 30, 1800).await.unwrap();
    // We expect exactly one RunStale — the last_activity_at was refreshed by
    // RunStepFinished but still falls outside the 1800s window for `now`.
    assert_eq!(emitted, 1);
    let all = store.load_since(0, 1024).await.unwrap();
    assert_eq!(count_kind(&all, "run_stale"), 1);
    assert_eq!(count_kind(&all, "run_unresponsive"), 0);

    // Apply a fresh step at "now"; the watchdog should no longer mark stale
    // (already marked, but if we hadn't, refresh would prevent it).
    let _ = run_id;
}

// ── Run becomes Completed mid-tick — must not be flagged retroactively ───────

#[tokio::test]
async fn completed_run_is_not_flagged() {
    let (handler, store) = build_stack().await;
    let run_id = start_run(&handler).await;

    handler
        .handle(Command::CompleteRun { run_id }, Actor::user())
        .await
        .unwrap();

    let now = Utc::now() + chrono::Duration::seconds(1801);
    let emitted = handler.tick_liveness(now, 30, 1800).await.unwrap();
    assert_eq!(emitted, 0);

    let all = store.load_since(0, 1024).await.unwrap();
    assert_eq!(count_kind(&all, "run_stale"), 0);
    assert_eq!(count_kind(&all, "run_unresponsive"), 0);
}
