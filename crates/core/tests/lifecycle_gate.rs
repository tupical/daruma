//! Lifecycle gate integration tests (docs/LIFECYCLE_RULES_SPEC.md §1.1/§1.5).
//!
//! Covers: trigger derivation from built events, warning pass-through into
//! `DispatchOutcome.warnings`, blocked-before-persist semantics (no events
//! appended, projection untouched), force propagation into `GateOverride`,
//! and zero-cost behaviour when no gate is wired.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use daruma_api_dto::MutationWarning;
use daruma_core::lifecycle_gate::{
    derive_gate_checks, GateCheck, GateDecision, GateOverride, LifecycleGate, TriggerEvent,
};
use daruma_core::{Command, CommandHandler};
use daruma_domain::{Actor, DocumentKind, NewDocument, NewTask, PlanStatus, Status};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::{CoreError, DocumentId, HandoffId, PlanId, ProjectId, RunId, TaskId};
use daruma_storage::{ActivityRepo, CommentRepo, Db, ProjectRepo, SqliteEventStore, TaskRepo};

/// Recording stub: allows everything unless `block_trigger` matches; can
/// emit a fixed warning on every check. When `identified` is set, decisions
/// carry a real `rule_id`/`rule_key` in `details` so the handler emits a
/// `RuleFired` audit event (the real engine always identifies its rules).
#[derive(Default)]
struct TestGate {
    block_trigger: Option<TriggerEvent>,
    warn: bool,
    identified: bool,
    seen: Mutex<Vec<(TriggerEvent, bool)>>,
}

const TEST_RULE_ID: &str = "rule_00000000-0000-7000-8000-000000000001";

#[async_trait]
impl LifecycleGate for TestGate {
    async fn check(
        &self,
        _actor: &Actor,
        check: &GateCheck,
        gate_override: &GateOverride,
    ) -> daruma_shared::Result<GateDecision> {
        self.seen
            .lock()
            .unwrap()
            .push((check.trigger, gate_override.force));
        let details = if self.identified {
            serde_json::json!({"rule_id": TEST_RULE_ID, "rule_key": "test.rule"})
        } else {
            serde_json::Value::Null
        };
        if self.block_trigger == Some(check.trigger) {
            return Ok(GateDecision::Blocked {
                message: format!("{} requires evidence", check.trigger.as_str()),
                details,
            });
        }
        if self.warn {
            return Ok(GateDecision::Warning(vec![MutationWarning {
                code: "rule_warning".to_string(),
                message: "test warning".to_string(),
                details,
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
        .handle(
            Command::CompleteTask {
                id: task_id,
                note: None,
            },
            Actor::user(),
        )
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
    assert!(
        !outcome.events.is_empty(),
        "mutation must persist on warning"
    );
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
        .handle(
            Command::CompleteTask {
                id: task_id,
                note: None,
            },
            Actor::user(),
        )
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
        .handle(
            Command::CompleteTask {
                id: task_id,
                note: None,
            },
            Actor::user(),
        )
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
    let now = daruma_shared::time::now();

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

#[test]
fn derive_checks_maps_document_created_and_task_handoff() {
    let project_id = ProjectId::new();
    let now = daruma_shared::time::now();
    let document = NewDocument {
        id: Some(DocumentId::new()),
        project_id,
        kind: DocumentKind::Interview,
        title: "Notes".to_string(),
        content: None,
        status: None,
        task_id: None,
        trigger_kind: None,
        consumer: None,
    }
    .into_document(DocumentId::new(), now);
    let document_id = document.id;
    let handoff_id = HandoffId::new();

    let events = vec![
        Event::DocumentCreated {
            document: document.clone(),
        },
        Event::HandoffAccepted {
            handoff_id,
            by: None,
            notes: None,
            latency_ms: Some(42),
            at: now,
        },
    ];

    let checks = derive_gate_checks(&events);
    assert_eq!(
        checks.iter().map(|c| c.trigger).collect::<Vec<_>>(),
        vec![TriggerEvent::DocumentCreated, TriggerEvent::TaskHandoff]
    );
    assert_eq!(checks[0].document_id, Some(document_id));
    assert_eq!(checks[0].project_id, Some(project_id));
    assert_eq!(checks[1].handoff_id, Some(handoff_id));
    // `HandoffAccepted` carries no work-unit/task ref, so this check cannot
    // be scoped narrower than tenant (see the derivation's doc comment).
    assert_eq!(checks[1].project_id, None);
    assert_eq!(checks[1].task_id, None);
}

// ── Completion note + rule-fired audit (OSS task 019eb65a-86d0) ───────────────

#[tokio::test]
async fn complete_task_without_note_is_backward_compatible() {
    // A legacy `CompleteTask { id }` (note omitted) emits the same three events
    // and carries no completion_note on TaskCompleted.
    let (handler, _store, tasks) = stack_with_gate(None).await;
    let task_id = create_task(&handler, "No note").await;

    let envs = handler
        .handle(
            Command::CompleteTask {
                id: task_id,
                note: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    assert_eq!(envs.len(), 3, "status_changed + completed + closed");
    let completed = envs
        .iter()
        .find_map(|e| match &e.payload {
            Event::TaskCompleted {
                completion_note, ..
            } => Some(completion_note.clone()),
            _ => None,
        })
        .expect("TaskCompleted present");
    assert!(completed.is_none(), "no note → None on the event");
    assert_eq!(
        tasks.get(task_id).await.unwrap().unwrap().status,
        Status::Done
    );
}

#[tokio::test]
async fn complete_task_with_note_carries_note_and_actor_kind() {
    use daruma_domain::CompletionNote;

    let (handler, _store, _tasks) = stack_with_gate(None).await;
    let task_id = create_task(&handler, "With note").await;

    let note = CompletionNote {
        reason: Some("acceptance criteria met".into()),
        result_summary: Some("shipped v1".into()),
        acceptance_criteria_status: Some("3/3 met".into()),
        related_artifacts: vec!["PR#42".into()],
        ..CompletionNote::default()
    };
    // Complete as an agent so the stamped actor_kind is "agent" (human-vs-agent
    // distinction is the task's audit risk note).
    let envs = handler
        .handle(
            Command::CompleteTask {
                id: task_id,
                note: Some(note),
            },
            Actor::agent("test-agent"),
        )
        .await
        .unwrap();

    let completion = envs
        .iter()
        .find_map(|e| match &e.payload {
            Event::TaskCompleted {
                completion_note, ..
            } => completion_note.clone(),
            _ => None,
        })
        .expect("note rides TaskCompleted");
    assert_eq!(
        completion.reason.as_deref(),
        Some("acceptance criteria met")
    );
    assert_eq!(completion.result_summary.as_deref(), Some("shipped v1"));
    assert_eq!(
        completion.acceptance_criteria_status.as_deref(),
        Some("3/3 met")
    );
    assert_eq!(completion.related_artifacts, vec!["PR#42".to_string()]);
    let actor = completion
        .actor
        .expect("handler stamps the completing actor");
    assert_eq!(actor.kind, "agent", "agent-self-reported completion");
    assert_eq!(actor.name.as_deref(), Some("test-agent"));
}

#[tokio::test]
async fn rule_fired_audit_persists_on_blocked_and_is_visible_in_event_log() {
    use daruma_events::event::RuleDecision;

    let gate = Arc::new(TestGate {
        block_trigger: Some(TriggerEvent::TaskBeforeComplete),
        identified: true,
        ..TestGate::default()
    });
    let (handler, store, tasks) = stack_with_gate(Some(gate)).await;

    let task_id = create_task(&handler, "Audited block").await;
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
    let before = store.load_since(0, 100).await.unwrap().len();

    let err = handler
        .handle(
            Command::CompleteTask {
                id: task_id,
                note: None,
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Conflict(_)));

    // The block left an audit trail (one RuleFired) even though the mutation
    // was rejected — but the task itself did NOT transition.
    let events = store.load_since(0, 100).await.unwrap();
    assert_eq!(
        events.len(),
        before + 1,
        "exactly one audit event persisted"
    );
    let fired = events
        .iter()
        .find_map(|e| match &e.payload {
            Event::RuleFired {
                decision,
                rule_key,
                task_id: t,
                ..
            } => Some((*decision, rule_key.clone(), *t)),
            _ => None,
        })
        .expect("RuleFired present");
    assert_eq!(fired.0, RuleDecision::Blocked);
    assert_eq!(fired.1, "test.rule");
    assert_eq!(fired.2, Some(task_id));
    assert_eq!(
        tasks.get(task_id).await.unwrap().unwrap().status,
        Status::InProgress,
        "blocked transition did not land"
    );
}

#[tokio::test]
async fn rule_fired_audit_rides_ahead_of_the_warned_mutation() {
    use daruma_events::event::RuleDecision;

    let gate = Arc::new(TestGate {
        warn: true,
        identified: true,
        ..TestGate::default()
    });
    let (handler, store, _tasks) = stack_with_gate(Some(gate)).await;

    let outcome = handler
        .handle_with_warnings(
            Command::CreateTask {
                task: NewTask::new("Warned + audited"),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.warnings.len(), 1);

    // RuleFired(warning) is persisted before the TaskCreated it warned on.
    let events = store.load_since(0, 100).await.unwrap();
    assert_eq!(events.len(), 2);
    match &events[0].payload {
        Event::RuleFired { decision, .. } => assert_eq!(*decision, RuleDecision::Warning),
        other => panic!("audit must precede the mutation, got {other:?}"),
    }
    assert!(matches!(events[1].payload, Event::TaskCreated { .. }));
}

#[tokio::test]
async fn allowed_decision_emits_no_audit_noise() {
    // identified=true but neither warn nor block → Allowed → no RuleFired.
    let gate = Arc::new(TestGate {
        identified: true,
        ..TestGate::default()
    });
    let (handler, store, _tasks) = stack_with_gate(Some(gate)).await;
    let _ = create_task(&handler, "Allowed").await;

    let events = store.load_since(0, 100).await.unwrap();
    assert!(
        events
            .iter()
            .all(|e| !matches!(e.payload, Event::RuleFired { .. })),
        "allowed transitions must not emit RuleFired"
    );
}
