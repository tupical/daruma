//! Integration tests for §3.8.2 — `RunNoteAppended` event + projection.
//!
//! Covers the command-handler invariants:
//!   * `AppendRunNote` emits exactly one `RunNoteAppended` event and writes a
//!     row into the `run_notes` projection.
//!   * Empty / oversized bodies are rejected at validation time (no event).
//!   * Unknown run → `NotFound`.
//!   * Terminal run (Completed / Aborted) → validation error.
//!   * `list_for_run` returns notes in chronological order.
//!   * Cursor-based pagination via `after` works across multiple pages.

use std::sync::Arc;

use daruma_core::{Command, CommandHandler};
use daruma_domain::{Actor, NewPlan, PlanStatus};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::{AgentId, CoreError, ProjectId, RunId};
use daruma_storage::{
    ActivityRepo, AgentClaimRepo, CommentRepo, Db, ExternalRefRepo, PlanRepo, ProjectRepo,
    RelationRepo, RunNoteRepo, RunRepo, SessionRepo, SqliteEventStore, TaskRepo,
};

async fn build_stack() -> (CommandHandler, Arc<dyn EventStore>, Arc<RunNoteRepo>) {
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
    let run_notes = Arc::new(RunNoteRepo::new(pool.clone()));
    let sessions = Arc::new(SessionRepo::new(pool.clone()));
    let claims = Arc::new(AgentClaimRepo::new(pool.clone()));
    let ext_refs = Arc::new(ExternalRefRepo::new(pool.clone()));
    let relations = Arc::new(RelationRepo::new(pool));
    let bus = EventBus::default();

    let handler = CommandHandler::new(store.clone(), tasks, projects, comments, activity, bus)
        .with_plans(plans)
        .with_runs(runs)
        .with_run_notes(run_notes.clone())
        .with_sessions(sessions)
        .with_claims(claims)
        .with_external_refs(ext_refs)
        .with_relations(relations);

    (handler, store, run_notes)
}

async fn start_active_run(handler: &CommandHandler) -> RunId {
    let envs = handler
        .handle(
            Command::CreatePlan {
                plan: NewPlan::new("Run-notes plan", ProjectId::new(), Actor::user()),
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

#[tokio::test]
async fn append_creates_note_and_event() {
    let (handler, store, notes_repo) = build_stack().await;
    let run_id = start_active_run(&handler).await;

    let actor = Actor::agent("integration-test");
    let envs = handler
        .handle(
            Command::AppendRunNote {
                run_id,
                body: "first observation".to_string(),
            },
            actor.clone(),
        )
        .await
        .unwrap();

    assert_eq!(envs.len(), 1);
    let (note_id, body, by_actor) = match &envs[0].payload {
        Event::RunNoteAppended {
            run_id: rid,
            note_id,
            body,
            by_actor,
            ..
        } => {
            assert_eq!(*rid, run_id);
            (*note_id, body.clone(), by_actor.clone())
        }
        other => panic!("expected RunNoteAppended, got {other:?}"),
    };
    assert_eq!(body, "first observation");
    assert_eq!(by_actor, actor);

    // Event store ↔ projection consistency.
    let all = store.load_since(0, 1024).await.unwrap();
    let count = all
        .iter()
        .filter(|e| e.payload.kind() == "run_note_appended")
        .count();
    assert_eq!(count, 1);

    let projected = notes_repo.list_for_run(run_id, 50, None).await.unwrap();
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].id, note_id);
    assert_eq!(projected[0].run_id, run_id);
    assert_eq!(projected[0].body, "first observation");
    assert_eq!(projected[0].author, actor);
}

#[tokio::test]
async fn append_rejects_empty_body() {
    let (handler, _store, _notes) = build_stack().await;
    let run_id = start_active_run(&handler).await;

    for body in ["", "   ", "\n\t  "] {
        let err = handler
            .handle(
                Command::AppendRunNote {
                    run_id,
                    body: body.to_string(),
                },
                Actor::user(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, CoreError::Validation(_)),
            "expected Validation for body {body:?}, got {err:?}",
        );
    }
}

#[tokio::test]
async fn append_rejects_oversized_body() {
    let (handler, _store, _notes) = build_stack().await;
    let run_id = start_active_run(&handler).await;

    // 4097 bytes — exceeds the 4 KiB cap by 1 byte.
    let body = "x".repeat(4097);
    let err = handler
        .handle(Command::AppendRunNote { run_id, body }, Actor::user())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoreError::Validation(_)),
        "expected Validation, got {err:?}",
    );

    // 4096 bytes — exactly at the cap is OK.
    let body = "y".repeat(4096);
    handler
        .handle(Command::AppendRunNote { run_id, body }, Actor::user())
        .await
        .expect("4096-byte body must be accepted");
}

#[tokio::test]
async fn append_rejects_unknown_run() {
    let (handler, _store, _notes) = build_stack().await;
    let err = handler
        .handle(
            Command::AppendRunNote {
                run_id: RunId::new(),
                body: "ghost".to_string(),
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoreError::NotFound(_)),
        "expected NotFound, got {err:?}",
    );
}

#[tokio::test]
async fn append_rejects_terminal_run() {
    let (handler, _store, _notes) = build_stack().await;
    let run_id = start_active_run(&handler).await;

    handler
        .handle(Command::CompleteRun { run_id }, Actor::user())
        .await
        .unwrap();

    let err = handler
        .handle(
            Command::AppendRunNote {
                run_id,
                body: "too late".to_string(),
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoreError::Validation(_)),
        "expected Validation for terminal run, got {err:?}",
    );
}

#[tokio::test]
async fn list_returns_notes_in_chronological_order() {
    let (handler, _store, notes_repo) = build_stack().await;
    let run_id = start_active_run(&handler).await;

    for i in 0..5 {
        handler
            .handle(
                Command::AppendRunNote {
                    run_id,
                    body: format!("note {i}"),
                },
                Actor::user(),
            )
            .await
            .unwrap();
        // Tiny sleep so two consecutive notes don't share the same ms-precision
        // timestamp on fast CI runners. UUIDv7 still tie-breaks but this makes
        // the test intent explicit.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    let listed = notes_repo.list_for_run(run_id, 50, None).await.unwrap();
    assert_eq!(listed.len(), 5);
    for (i, note) in listed.iter().enumerate() {
        assert_eq!(note.body, format!("note {i}"));
    }
}

#[tokio::test]
async fn list_paginates_via_after_cursor() {
    let (handler, _store, notes_repo) = build_stack().await;
    let run_id = start_active_run(&handler).await;

    for i in 0..5 {
        handler
            .handle(
                Command::AppendRunNote {
                    run_id,
                    body: format!("note {i}"),
                },
                Actor::user(),
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    let page1 = notes_repo.list_for_run(run_id, 2, None).await.unwrap();
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].body, "note 0");

    let page2 = notes_repo
        .list_for_run(run_id, 2, Some(page1[1].id))
        .await
        .unwrap();
    assert_eq!(page2.len(), 2);
    assert_eq!(page2[0].body, "note 2");

    let page3 = notes_repo
        .list_for_run(run_id, 2, Some(page2[1].id))
        .await
        .unwrap();
    assert_eq!(page3.len(), 1);
    assert_eq!(page3[0].body, "note 4");
}
