mod common;

use axum::http::StatusCode;
use common::{json_post, mint_with_caps, test_app};
use daruma_auth::{Capabilities, TokenKind};
use daruma_core::Command;
use daruma_domain::{Actor, DocumentKind, DocumentStatus, NewDocument, NewTask};
use daruma_events::Event;

#[tokio::test]
async fn admin_sweep_archives_orphans_through_the_event_log() {
    let app = test_app().await;
    let handler = app.state.commands.handler();
    let project_id = handler
        .handle(
            Command::CreateProject {
                title: "Sweep".into(),
                description: None,
            },
            Actor::user(),
        )
        .await
        .unwrap()
        .into_iter()
        .find_map(|env| match env.payload {
            Event::ProjectCreated { project } => Some(project.id),
            _ => None,
        })
        .unwrap();
    let mut task = NewTask::new("Anchor");
    task.project_id = Some(project_id);
    let task_id = handler
        .handle(Command::CreateTask { task }, Actor::user())
        .await
        .unwrap()
        .into_iter()
        .find_map(|env| match env.payload {
            Event::TaskCreated { task } => task.id,
            _ => None,
        })
        .unwrap();
    let doc_id = handler
        .handle(
            Command::CreateDocument {
                new_doc: NewDocument {
                    id: None,
                    project_id,
                    kind: DocumentKind::Interview,
                    title: "Orphan".into(),
                    content: None,
                    status: None,
                    task_id: Some(task_id),
                    trigger_kind: None,
                    consumer: None,
                },
            },
            Actor::user(),
        )
        .await
        .unwrap()
        .into_iter()
        .find_map(|env| match env.payload {
            Event::DocumentCreated { document } => Some(document.id),
            _ => None,
        })
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
    handler
        .handle(
            Command::SetDocumentStatus {
                document_id: doc_id,
                status: DocumentStatus::Active,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    let (plain, _) =
        mint_with_caps(&app.auth_store(), TokenKind::Pat, Capabilities::default()).await;
    let (status, _) = json_post(
        app.router.clone(),
        &plain,
        "/v1/admin/documents/sweep",
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, body) = json_post(
        app.router.clone(),
        &app.admin_token,
        "/v1/admin/documents/sweep",
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["swept"], 1);
    assert_eq!(
        app.state
            .documents
            .get(doc_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DocumentStatus::Archived
    );
}
