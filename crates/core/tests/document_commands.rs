//! Integration tests for PR1 §5-6 — Document commands.
//!
//! Covers:
//!   * `Command::CreateProject` emits only `ProjectCreated` — the core no
//!     longer auto-seeds narrative Interview / Human Log documents.
//!   * Each Document command emits the corresponding event and updates state.
//!   * Validation: empty titles rejected; unknown document → NotFound;
//!     archiving an already-archived document is a no-op.

use std::sync::Arc;

use daruma_core::{Command, CommandHandler};
use daruma_domain::{
    Actor, Document, DocumentKind, NewDocument,
};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::{CoreError, DocumentId, ProjectId};
use daruma_storage::{
    ActivityRepo, AgentClaimRepo, CommentRepo, Db, DocumentRepo, ExternalRefRepo, PlanRepo,
    ProjectRepo, RelationRepo, RunNoteRepo, RunRepo, SessionRepo, SqliteEventStore, TaskRepo,
};

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

/// Create a project and return its id, asserting `CreateProject` emits only
/// `ProjectCreated` (no auto-seeded documents).
async fn create_bare_project(handler: &CommandHandler) -> ProjectId {
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
    assert_eq!(envs.len(), 1, "CreateProject emits only ProjectCreated");
    match &envs[0].payload {
        Event::ProjectCreated { project } => project.id,
        other => panic!("expected ProjectCreated, got {other:?}"),
    }
}

/// Create one document under `project_id` and return its id.
async fn create_doc(
    handler: &CommandHandler,
    project_id: ProjectId,
    kind: DocumentKind,
    title: &str,
) -> DocumentId {
    let envs = handler
        .handle(
            Command::CreateDocument {
                new_doc: NewDocument {
                    id: None,
                    project_id,
                    kind,
                    title: title.into(),
                    content: None,
                },
            },
            Actor::user(),
        )
        .await
        .unwrap();
    match &envs[0].payload {
        Event::DocumentCreated { document } => document.id,
        other => panic!("expected DocumentCreated, got {other:?}"),
    }
}

/// `CreateProject` must emit only `ProjectCreated`: the execution core no
/// longer auto-seeds narrative Interview / Human Log documents, so a fresh
/// project starts with an empty document projection.
#[tokio::test]
async fn create_project_does_not_seed_documents() {
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

    assert_eq!(envs.len(), 1, "expected only the ProjectCreated event");
    let project_id = match &envs[0].payload {
        Event::ProjectCreated { project } => project.id,
        other => panic!("expected ProjectCreated, got {other:?}"),
    };

    let docs = documents
        .list_by_project(project_id, None, false)
        .await
        .unwrap();
    assert!(
        docs.is_empty(),
        "fresh project must have no auto-seeded documents: {docs:?}"
    );
}

/// `CreateDocument` with explicit kind emits a single `DocumentCreated` and
/// the projection reflects it. Multiple docs of the same kind are allowed.
#[tokio::test]
async fn create_document_emits_event_and_allows_duplicate_kind() {
    let (handler, documents) = build_stack().await;

    let project_id = create_bare_project(&handler).await;

    // First Interview document (created explicitly — nothing is auto-seeded).
    create_doc(&handler, project_id, DocumentKind::Interview, "First Interview").await;

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

    let project_id = create_bare_project(&handler).await;
    let interview_id =
        create_doc(&handler, project_id, DocumentKind::Interview, "Interview").await;

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

    let project_id = create_bare_project(&handler).await;
    let interview_id =
        create_doc(&handler, project_id, DocumentKind::Interview, "Interview").await;

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

    let project_id = create_bare_project(&handler).await;
    let interview_id =
        create_doc(&handler, project_id, DocumentKind::Interview, "Interview").await;

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
