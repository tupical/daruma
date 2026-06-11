//! Lifecycle gate integration tests (docs/LIFECYCLE_RULES_SPEC.md §1.1/§1.5).
//!
//! Covers: trigger derivation from built events, warning pass-through into
//! `DispatchOutcome.warnings`, blocked-before-persist semantics (no events
//! appended, projection untouched), force propagation into `GateOverride`,
//! and zero-cost behaviour when no gate is wired.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use taskagent_api_dto::MutationWarning;
use taskagent_core::lifecycle_gate::{
    derive_gate_checks, GateCheck, GateDecision, GateOverride, LifecycleGate, TriggerEvent,
};
use taskagent_core::{Command, CommandHandler};
use taskagent_domain::{Actor, NewTask, PlanStatus, Status};
use taskagent_events::{Event, EventBus, EventStore};
use taskagent_shared::{CoreError, PlanId, RunId, TaskId};
use taskagent_storage::{ActivityRepo, CommentRepo, Db, ProjectRepo, SqliteEventStore, TaskRepo};

/// Recording stub: allows everything unless `block_trigger` matches; can
/// emit a fixed warning on every check.
#[derive(Default)]
struct TestGate {
    block_trigger: Option<TriggerEvent>,
    warn: bool,
    seen: Mutex<Vec<(TriggerEvent, bool)>>,
}

#[async_trait]
impl LifecycleGate for TestGate {
    async fn check(
        &self,
        _actor: &Actor,
        check: &GateCheck,
        gate_override: &GateOverride,
    ) -> taskagent_shared::Result<GateDecision> {
        self.seen
            .lock()
            .unwrap()
            .push((check.trigger, gate_override.force));
        if self.block_trigger == Some(check.trigger) {
            return Ok(GateDecision::Blocked {
                message: format!("{} requires evidence", check.trigger.as_str()),
                details: serde_json::Value::Null,
            });
        }
        if self.warn {
            return Ok(GateDecision::Warning(vec![MutationWarning {
                code: "rule_warning".to_string(),
                message: "test warning".to_string(),
                details: serde_json::Value::Null,
            }]));
        }
        Ok(GateDecision::Allowed)
    }
}

async fn stack_with_gate(
    gate: Option<Arc<TestGate>>,
) -> (CommandHandler, Arc<dyn EventStore>, Arc<TaskRepo>) {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let tasks = Arc::new(TaskRepo::new(pool.clone()));
    let projects = Arc::new(ProjectRepo::new(pool.clone()));
    let comments = Arc::new(CommentRepo::new(pool.clone()));
    let activity = Arc::new(ActivityRepo::new(pool));
    let mut handler = CommandHandler::new(
        store.clone(),
        tasks.clone(),
        projects,
        comments,
        activity,
        EventBus::default(),
    );
    if let Some(gate) = gate {
        handler = handler.with_lifecycle_gate(gate);
    }
    (handler, store, tasks)
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

#[tokio::test]
async fn gate_sees_task_and_project_triggers_with_force() {
    let gate = Arc::new(TestGate::default());
    let (handler, _store, _tasks) = stack_with_gate(Some(gate.clone())).await;

    handler
        .handle(
            Command::CreateProject {
                title: "Gated project".to_string(),
                description: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let task_id = create_task(&handler, "Gated task").await;
    handler
        .handle(
            Command::SetStatus {
                id: task_id,
                status: Status::InProgress,
                force: true,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    handler
        .handle(Command::CompleteTask { id: task_id }, Actor::user())
        .await
        .unwrap();

    let seen = gate.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            (TriggerEvent::ProjectCreated, false),
            (TriggerEvent::TaskCreated, false),
            (TriggerEvent::TaskBeforeStart, true),
            (TriggerEvent::TaskBeforeComplete, false),
        ]
    );
}

#[tokio::test]
async fn warning_rides_dispatch_outcome_and_persists() {
    let gate = Arc::new(TestGate {
        warn: true,
        ..TestGate::default()
    });
    let (handler, store, _tasks) = stack_with_gate(Some(gate)).await;

    let outcome = handler
        .handle_with_warnings(
            Command::CreateTask {
                task: NewTask::new("Warned but allowed"),
            },
            Actor::user(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.warnings.len(), 1);
    assert_eq!(outcome.warnings[0].code, "rule_warning");
    assert!(!outcome.events.is_empty(), "mutation must persist on warning");
    assert_eq!(store.load_since(0, 100).await.unwrap().len(), 1);
}

#[tokio::test]
async fn blocked_aborts_before_persist_on_both_complete_paths() {
    let gate = Arc::new(TestGate {
        block_trigger: Some(TriggerEvent::TaskBeforeComplete),
        ..TestGate::default()
    });
    let (handler, store, tasks) = stack_with_gate(Some(gate)).await;

    let task_id = create_task(&handler, "Blocked completion").await;
    handler
        .handle(
            Command::SetStatus {
                id: task_id,
                status: Status::InProgress,
                force: false,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let events_before = store.load_since(0, 100).await.unwrap().len();

    // Path 1: CompleteTask.
    let err = handler
        .handle(Command::CompleteTask { id: task_id }, Actor::user())
        .await
        .unwrap_err();
    match err {
        CoreError::Conflict(msg) => assert!(
            msg.contains("rule_blocked"),
            "conflict message must contain 'rule_blocked', got: {msg}"
        ),
        other => panic!("expected Conflict, got {other:?}"),
    }

    // Path 2: SetStatus(done) — same derived trigger, same gate.
    let err = handler
        .handle(
            Command::SetStatus {
                id: task_id,
                status: Status::Done,
                force: false,
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Conflict(_)));

    // Nothing persisted, projection untouched (spec §3, invariant 7: gate
    // sits before persist).
    assert_eq!(store.load_since(0, 100).await.unwrap().len(), events_before);
    let task = tasks.get(task_id).await.unwrap().unwrap();
    assert_eq!(task.status, Status::InProgress);
    assert!(task.completed_at.is_none());
}

#[tokio::test]
async fn no_gate_keeps_existing_behavior() {
    let (handler, _store, tasks) = stack_with_gate(None).await;
    let task_id = create_task(&handler, "Ungated").await;
    handler
        .handle(Command::CompleteTask { id: task_id }, Actor::user())
        .await
        .unwrap();
    let task = tasks.get(task_id).await.unwrap().unwrap();
    assert_eq!(task.status, Status::Done);
}

#[test]
fn derive_checks_maps_transitions_and_skips_non_lifecycle_events() {
    let plan_id = PlanId::new();
    let run_id = RunId::new();
    let task_id = TaskId::new();
    let now = taskagent_shared::time::now();

    let events = vec![
        Event::PlanStatusChanged {
            plan_id,
            from: PlanStatus::Draft,
            to: PlanStatus::Active,
        },
        // Active → Completed is NOT before_approve.
        Event::PlanStatusChanged {
            plan_id,
            from: PlanStatus::Active,
            to: PlanStatus::Completed,
        },
        Event::RunCompleted { run_id, at: now },
        Event::TaskStatusChanged {
            task_id,
            from: Status::Todo,
            to: Status::InProgress,
        },
        Event::TaskStatusChanged {
            task_id,
            from: Status::InProgress,
            to: Status::Done,
        },
        // InReview is neither start nor complete in v1.
        Event::TaskStatusChanged {
            task_id,
            from: Status::Todo,
            to: Status::InReview,
        },
    ];

    let triggers: Vec<TriggerEvent> = derive_gate_checks(&events)
        .into_iter()
        .map(|c| c.trigger)
        .collect();
    assert_eq!(
        triggers,
        vec![
            TriggerEvent::PlanBeforeApprove,
            TriggerEvent::RunBeforeComplete,
            TriggerEvent::TaskBeforeStart,
            TriggerEvent::TaskBeforeComplete,
        ]
    );

    let checks = derive_gate_checks(&events);
    assert_eq!(checks[0].plan_id, Some(plan_id));
    assert_eq!(checks[0].plan_status_from, Some(PlanStatus::Draft));
    assert_eq!(checks[1].run_id, Some(run_id));
    assert_eq!(checks[2].status_to, Some(Status::InProgress));
    assert_eq!(checks[3].status_from, Some(Status::InProgress));
}
