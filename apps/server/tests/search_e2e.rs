use axum::http::StatusCode;
use serde_json::json;
use daruma_core::Command;
use daruma_domain::{Actor, NewComment, NewPlan, NewTask};
use daruma_events::Event;
use daruma_shared::TaskId;

mod common;
use common::{json_get, test_app};

#[tokio::test]
async fn search_returns_tasks_comments_and_plans() {
    let app = test_app().await;
    let actor = Actor::user();

    let project_envs = app
        .state
        .commands
        .dispatch(
            Command::CreateProject {
                title: "Search Project".to_string(),
                description: None,
            },
            actor.clone(),
        )
        .await
        .unwrap();
    let project_id = match &project_envs[0].payload {
        Event::ProjectCreated { project } => project.id,
        other => panic!("expected ProjectCreated, got {other:?}"),
    };

    let task_id = TaskId::new();
    let mut task = NewTask::new("Needle task");
    task.id = Some(task_id);
    task.project_id = Some(project_id);
    app.state
        .commands
        .dispatch(Command::CreateTask { task }, actor.clone())
        .await
        .unwrap();

    app.state
        .commands
        .dispatch(
            Command::AddComment {
                comment: NewComment {
                    id: None,
                    task_id,
                    body: "needle comment body".to_string(),
                    parent_id: None,
                    kind: None,
                },
            },
            actor.clone(),
        )
        .await
        .unwrap();

    let mut plan = NewPlan::new("Needle plan", project_id, actor.clone());
    plan.goal = Some("Find the same needle".to_string());
    app.state
        .commands
        .dispatch(
            Command::CreatePlan {
                plan,
                external_ref: None,
            },
            actor,
        )
        .await
        .unwrap();

    let (status, body) = json_get(
        app.router,
        &app.admin_token,
        &format!("/v1/search?query=needle&scope=tasks,comments,plans&project_id={project_id}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "search response: {body}");
    let kinds = body
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(kinds, vec!["task", "comment", "plan"]);
    assert_eq!(body[0]["task_id"], json!(task_id));
    assert_eq!(body[1]["task_id"], json!(task_id));
    assert_eq!(body[2]["project_id"], json!(project_id));
}

#[tokio::test]
async fn search_branch_query_returns_matching_branch_comments() {
    let app = test_app().await;
    let actor = Actor::user();

    let project_envs = app
        .state
        .commands
        .dispatch(
            Command::CreateProject {
                title: "Branch Search Project".to_string(),
                description: None,
            },
            actor.clone(),
        )
        .await
        .unwrap();
    let project_id = match &project_envs[0].payload {
        Event::ProjectCreated { project } => project.id,
        other => panic!("expected ProjectCreated, got {other:?}"),
    };

    let task_id = TaskId::new();
    let mut task = NewTask::new("Feature branch task");
    task.id = Some(task_id);
    task.project_id = Some(project_id);
    app.state
        .commands
        .dispatch(Command::CreateTask { task }, actor.clone())
        .await
        .unwrap();

    app.state
        .commands
        .dispatch(
            Command::AddComment {
                comment: NewComment {
                    id: None,
                    task_id,
                    body: "branch: feature/mcp-search active work".to_string(),
                    parent_id: None,
                    kind: None,
                },
            },
            actor.clone(),
        )
        .await
        .unwrap();
    app.state
        .commands
        .dispatch(
            Command::AddComment {
                comment: NewComment {
                    id: None,
                    task_id,
                    body: "branch: other active work".to_string(),
                    parent_id: None,
                    kind: None,
                },
            },
            actor,
        )
        .await
        .unwrap();

    let (status, body) = json_get(
        app.router,
        &app.admin_token,
        &format!(
            "/v1/search?query=branch:%20feature/mcp-search&scope=tasks,comments,plans&project_id={project_id}"
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "search response: {body}");
    let hits = body.as_array().unwrap();
    assert_eq!(hits.len(), 1, "branch search hits: {body}");
    assert_eq!(hits[0]["kind"], json!("comment"));
    assert_eq!(hits[0]["task_id"], json!(task_id));
    assert_eq!(
        hits[0]["snippet"],
        json!("branch: feature/mcp-search active work")
    );
}

#[tokio::test]
async fn search_lesson_query_returns_lesson_comments() {
    let app = test_app().await;
    let actor = Actor::user();

    let project_envs = app
        .state
        .commands
        .dispatch(
            Command::CreateProject {
                title: "Lesson Search Project".to_string(),
                description: None,
            },
            actor.clone(),
        )
        .await
        .unwrap();
    let project_id = match &project_envs[0].payload {
        Event::ProjectCreated { project } => project.id,
        other => panic!("expected ProjectCreated, got {other:?}"),
    };

    let task_id = TaskId::new();
    let mut task = NewTask::new("Search lessons");
    task.id = Some(task_id);
    task.project_id = Some(project_id);
    app.state
        .commands
        .dispatch(Command::CreateTask { task }, actor.clone())
        .await
        .unwrap();

    for body in [
        "lesson: branch comments should be indexed",
        "progress: branch comments should be indexed",
    ] {
        app.state
            .commands
            .dispatch(
                Command::AddComment {
                    comment: NewComment {
                        id: None,
                        task_id,
                        body: body.to_string(),
                        parent_id: None,
                        kind: None,
                    },
                },
                actor.clone(),
            )
            .await
            .unwrap();
    }

    let (status, body) = json_get(
        app.router,
        &app.admin_token,
        &format!(
            "/v1/search?query=lesson:%20branch&scope=tasks,comments,plans&project_id={project_id}"
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "search response: {body}");
    let hits = body.as_array().unwrap();
    assert_eq!(hits.len(), 1, "lesson search hits: {body}");
    assert_eq!(hits[0]["kind"], json!("comment"));
    assert_eq!(
        hits[0]["snippet"],
        json!("lesson: branch comments should be indexed")
    );
}
