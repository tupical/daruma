//! Integration tests for PR1 §5-6 — Document commands + auto-template.
//!
//! Covers:
//!   * `Command::CreateProject` emits `ProjectCreated` + two `DocumentCreated`
//!     events (Interview + HumanLog), and both rows appear in the projection.
//!   * Each Document command emits the corresponding event and updates state.
//!   * Validation: empty titles rejected; unknown document → NotFound;
//!     archiving an already-archived document is a no-op.

use std::sync::Arc;

use daruma_core::{Command, CommandHandler};
use daruma_domain::{
    Actor, Document, DocumentKind, NewDocument, NewPlan, NewTask, PlanStatus, Priority, Status,
};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::{CoreError, DocumentId, ProjectId};
use daruma_storage::{
    ActivityRepo, AgentClaimRepo, CommentRepo, Db, DocumentRepo, ExternalRefRepo, PlanRepo,
    ProjectRepo, RelationRepo, RunNoteRepo, RunRepo, SessionRepo, SqliteEventStore, TaskRepo,
};

// Silence unused-import warnings for items only used in some tests.
#[allow(unused_imports)]
use daruma_shared::TaskId;

async fn build_stack() -> (CommandHandler, Arc<DocumentRepo>) {
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
    let relations = Arc::new(RelationRepo::new(pool.clone()));
    let documents = Arc::new(DocumentRepo::new(pool));
    let bus = EventBus::default();

    let handler = CommandHandler::new(store, tasks, projects, comments, activity, bus)
        .with_plans(plans)
        .with_runs(runs)
        .with_run_notes(run_notes)
        .with_sessions(sessions)
        .with_claims(claims)
        .with_external_refs(ext_refs)
        .with_relations(relations)
        .with_documents(documents.clone());

    (handler, documents)
}

// Suppress unused-warnings for imports only some tests use.
#[allow(dead_code)]
fn _silence(_: NewTask, _: NewPlan, _: PlanStatus, _: Priority, _: Status) {}

/// `CreateProject` must emit `ProjectCreated` plus two `DocumentCreated`
/// events (Interview + HumanLog), and the projection must contain both rows.
#[tokio::test]
async fn create_project_seeds_interview_and_human_log() {
    let (handler, documents) = build_stack().await;

    let envs = handler
        .handle(
            Command::CreateProject {
                title: "Demo".into(),
                description: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    assert_eq!(envs.len(), 3, "expected 3 events (project + 2 documents)");
    let project_id = match &envs[0].payload {
        Event::ProjectCreated { project } => project.id,
        other => panic!("expected ProjectCreated, got {other:?}"),
    };
    let mut kinds: Vec<DocumentKind> = vec![];
    for env in &envs[1..] {
        match &env.payload {
            Event::DocumentCreated { document } => {
                assert_eq!(
                    document.project_id, project_id,
                    "doc must belong to project"
                );
                assert!(document.archived_at.is_none(), "auto-doc not archived");
                kinds.push(document.kind);
            }
            other => panic!("expected DocumentCreated, got {other:?}"),
        }
    }
    kinds.sort_by_key(|k| k.as_str());
    assert_eq!(
        kinds,
        vec![DocumentKind::HumanLog, DocumentKind::Interview],
        "both default kinds emitted"
    );

    let docs = documents
        .list_by_project(project_id, None, false)
        .await
        .unwrap();
    assert_eq!(docs.len(), 2, "both docs projected");
    let interview = docs
        .iter()
        .find(|d| d.kind == DocumentKind::Interview)
        .expect("Interview in projection");
    assert_eq!(interview.title, "Interview");
    assert_eq!(interview.content, "", "Interview body empty");
    let human_log = docs
        .iter()
        .find(|d| d.kind == DocumentKind::HumanLog)
        .expect("HumanLog in projection");
    assert_eq!(human_log.title, "Human Log");
    assert!(
        human_log.content.starts_with("# Human Log"),
        "HumanLog body has header: {:?}",
        human_log.content
    );
    assert!(
        human_log.content.contains("_Created "),
        "HumanLog body has created stamp: {:?}",
        human_log.content
    );
}

/// `CreateDocument` with explicit kind emits a single `DocumentCreated` and
/// the projection reflects it. Multiple docs of the same kind are allowed.
#[tokio::test]
async fn create_document_emits_event_and_allows_duplicate_kind() {
    let (handler, documents) = build_stack().await;

    let envs = handler
        .handle(
            Command::CreateProject {
                title: "Demo".into(),
                description: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let project_id = match &envs[0].payload {
        Event::ProjectCreated { project } => project.id,
        _ => unreachable!(),
    };

    // Second Interview document — kind is not unique per project.
    let envs = handler
        .handle(
            Command::CreateDocument {
                new_doc: NewDocument {
                    id: None,
                    project_id,
                    kind: DocumentKind::Interview,
                    title: "Second Interview".into(),
                    content: Some("notes".into()),
                },
            },
            Actor::user(),
        )
        .await
        .unwrap();
    assert_eq!(envs.len(), 1);
    let doc = match &envs[0].payload {
        Event::DocumentCreated { document } => document.clone(),
        other => panic!("expected DocumentCreated, got {other:?}"),
    };
    assert_eq!(doc.title, "Second Interview");
    assert_eq!(doc.content, "notes");

    let interviews = documents
        .list_by_project(project_id, Some(DocumentKind::Interview), false)
        .await
        .unwrap();
    assert_eq!(interviews.len(), 2, "two Interview docs now exist");
}

/// Empty title is rejected at validation time, before any event is emitted.
#[tokio::test]
async fn create_document_rejects_empty_title() {
    let (handler, _documents) = build_stack().await;

    let err = handler
        .handle(
            Command::CreateDocument {
                new_doc: NewDocument {
                    id: None,
                    project_id: ProjectId::new(),
                    kind: DocumentKind::Interview,
                    title: "   ".into(),
                    content: None,
                },
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoreError::Validation { .. }),
        "expected Validation, got {err:?}"
    );
}

/// `AppendDocumentContent` requires the document to exist; unknown id → NotFound.
#[tokio::test]
async fn append_unknown_document_is_not_found() {
    let (handler, _documents) = build_stack().await;

    let err = handler
        .handle(
            Command::AppendDocumentContent {
                document_id: DocumentId::new(),
                append: "x".into(),
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoreError::NotFound { .. }),
        "expected NotFound, got {err:?}"
    );
}

/// `RenameDocument` updates the projected title.
#[tokio::test]
async fn rename_document_updates_projection() {
    let (handler, documents) = build_stack().await;

    let envs = handler
        .handle(
            Command::CreateProject {
                title: "Demo".into(),
                description: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let interview_id = match &envs[1].payload {
        Event::DocumentCreated { document } => document.id,
        _ => unreachable!(),
    };

    handler
        .handle(
            Command::RenameDocument {
                document_id: interview_id,
                title: "Discovery Interview".into(),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let after = documents.get(interview_id).await.unwrap().unwrap();
    assert_eq!(after.title, "Discovery Interview");
}

/// `ReplaceDocumentContent` swaps the body; `AppendDocumentContent` extends it.
#[tokio::test]
async fn replace_then_append_compose() {
    let (handler, documents) = build_stack().await;

    let envs = handler
        .handle(
            Command::CreateProject {
                title: "Demo".into(),
                description: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let interview_id = match &envs[1].payload {
        Event::DocumentCreated { document } => document.id,
        _ => unreachable!(),
    };

    handler
        .handle(
            Command::ReplaceDocumentContent {
                document_id: interview_id,
                content: "first body".into(),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    handler
        .handle(
            Command::AppendDocumentContent {
                document_id: interview_id,
                append: "second".into(),
            },
            Actor::user(),
        )
        .await
        .unwrap();

    let Document { content, .. } = documents.get(interview_id).await.unwrap().unwrap();
    assert!(
        content.contains("first body") && content.contains("second"),
        "both segments in body: {content:?}"
    );
}

/// Archiving an already-archived document is a no-op (no second event).
#[tokio::test]
async fn archive_is_idempotent_on_already_archived() {
    let (handler, _documents) = build_stack().await;

    let envs = handler
        .handle(
            Command::CreateProject {
                title: "Demo".into(),
                description: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let interview_id = match &envs[1].payload {
        Event::DocumentCreated { document } => document.id,
        _ => unreachable!(),
    };

    let first = handler
        .handle(
            Command::ArchiveDocument {
                document_id: interview_id,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    assert_eq!(first.len(), 1, "first archive emits exactly one event");

    let second = handler
        .handle(
            Command::ArchiveDocument {
                document_id: interview_id,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    assert!(second.is_empty(), "re-archive is a no-op");
}
