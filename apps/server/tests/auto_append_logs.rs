//! Auto-append into the auto-created Interview / Human Log documents:
//! agent activity → Interview, human milestones → Human Log, toggleable
//! per project (ON by default) via /v1/projects/{id}/settings.

use serde_json::json;
use daruma_core::Command;
use daruma_domain::{Actor, AutoAppendPatch, DocumentKind, NewTask};
use daruma_shared::{AgentId, ProjectId};

mod common;
use common::test_app;

async fn create_project(h: &common::TestApp, title: &str) -> ProjectId {
    let envs = h
        .state
        .commands
        .handler()
        .handle(
            Command::CreateProject {
                title: title.into(),
                description: None,
            },
            Actor::user(),
        )
        .await
        .expect("create project");
    envs.iter()
        .find_map(|e| match &e.payload {
            daruma_events::Event::ProjectCreated { project } => Some(project.id),
            _ => None,
        })
        .expect("project id")
}

async fn doc_body(h: &common::TestApp, project: ProjectId, kind: DocumentKind) -> String {
    let docs = h
        .state
        .documents
        .list_by_project(project, Some(kind), false)
        .await
        .expect("list docs");
    let doc = docs.first().expect("auto-created doc exists");
    h.state
        .documents
        .get(doc.id)
        .await
        .expect("get doc")
        .expect("doc")
        .content
}

fn task_in(project: ProjectId, title: &str) -> NewTask {
    let mut t = NewTask::new(title);
    t.project_id = Some(project);
    t
}

#[tokio::test]
async fn user_task_lands_in_human_log_and_agent_task_in_interview() {
    let h = test_app().await;
    let handler = h.state.commands.handler();
    let project = create_project(&h, "Logged Project").await;

    handler
        .handle(
            Command::CreateTask {
                task: task_in(project, "Fix login bug"),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    handler
        .handle(
            Command::CreateTask {
                task: task_in(project, "Refactor session store"),
            },
            Actor::Agent {
                id: AgentId::new(),
                name: "executor-1".into(),
            },
        )
        .await
        .unwrap();

    let human = doc_body(&h, project, DocumentKind::HumanLog).await;
    assert!(
        human.contains("Created task 'Fix login bug'"),
        "human log: {human}"
    );
    assert!(
        !human.contains("Refactor session store"),
        "agent activity must not leak into the human log: {human}"
    );

    let interview = doc_body(&h, project, DocumentKind::Interview).await;
    assert!(
        interview.contains("agent=executor-1")
            && interview.contains("action=task_created")
            && interview.contains("Refactor session store"),
        "interview: {interview}"
    );
    assert!(
        !interview.contains("Fix login bug"),
        "user activity must not leak into the interview log: {interview}"
    );
}

#[tokio::test]
async fn toggles_disable_appends_and_persist() {
    let h = test_app().await;
    let handler = h.state.commands.handler();
    let project = create_project(&h, "Quiet Project").await;

    // Default is ON.
    let s = h.state.project_settings.auto_append(project).await.unwrap();
    assert!(s.interview && s.human_log);

    // Turn the human log off (partial patch keeps interview ON).
    handler
        .handle(
            Command::UpdateProjectSettings {
                project_id: project,
                auto_append: AutoAppendPatch {
                    interview: None,
                    human_log: Some(false),
                },
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let s = h.state.project_settings.auto_append(project).await.unwrap();
    assert!(s.interview, "partial patch must not touch interview");
    assert!(!s.human_log);

    handler
        .handle(
            Command::CreateTask {
                task: task_in(project, "Silent task"),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let human = doc_body(&h, project, DocumentKind::HumanLog).await;
    assert!(
        !human.contains("Silent task"),
        "disabled human log must stay silent: {human}"
    );

    // Re-enable → new lines appear again.
    handler
        .handle(
            Command::UpdateProjectSettings {
                project_id: project,
                auto_append: AutoAppendPatch {
                    interview: None,
                    human_log: Some(true),
                },
            },
            Actor::user(),
        )
        .await
        .unwrap();
    handler
        .handle(
            Command::CreateTask {
                task: task_in(project, "Loud task"),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let human = doc_body(&h, project, DocumentKind::HumanLog).await;
    assert!(human.contains("Created task 'Loud task'"), "{human}");
}

#[tokio::test]
async fn status_changes_route_by_actor() {
    let h = test_app().await;
    let handler = h.state.commands.handler();
    let project = create_project(&h, "Status Project").await;

    let envs = handler
        .handle(
            Command::CreateTask {
                task: task_in(project, "Track me"),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let task_id = envs
        .iter()
        .find_map(|e| match &e.payload {
            daruma_events::Event::TaskCreated { task } => task.id,
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

    let human = doc_body(&h, project, DocumentKind::HumanLog).await;
    assert!(
        human.contains("Track me") && human.contains("Done"),
        "completion shows up as a human milestone: {human}"
    );
}

#[tokio::test]
async fn settings_endpoint_shapes() {
    use axum::{
        body::{to_bytes, Body},
        http::{Method, Request, StatusCode},
    };
    use tower::ServiceExt;

    let h = test_app().await;
    let project = create_project(&h, "API Project").await;

    let get = Request::builder()
        .method(Method::GET)
        .uri(format!("/v1/projects/{project}/settings"))
        .header("authorization", format!("Bearer {}", h.admin_token))
        .body(Body::empty())
        .unwrap();
    let res = h.router.clone().oneshot(get).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["auto_append"]["interview"], true);
    assert_eq!(body["auto_append"]["human_log"], true);

    let patch = Request::builder()
        .method(Method::PATCH)
        .uri(format!("/v1/projects/{project}/settings"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", h.admin_token))
        .body(Body::from(
            json!({ "auto_append": { "interview": false } }).to_string(),
        ))
        .unwrap();
    let res = h.router.clone().oneshot(patch).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["data"]["auto_append"]["interview"], false);
    assert_eq!(body["data"]["auto_append"]["human_log"], true);
}
