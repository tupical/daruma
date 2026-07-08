use axum::http::StatusCode;
use daruma_core::Command;
use daruma_domain::{Actor, NewPlan, NewTask};
use daruma_events::Event;

mod common;
use common::{json_get, test_app};

async fn seed_project(app: &common::TestApp) -> daruma_shared::ProjectId {
    let envs = app
        .state
        .commands
        .dispatch(
            Command::CreateProject {
                title: "Limit Project".to_string(),
                description: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    match &envs[0].payload {
        Event::ProjectCreated { project } => project.id,
        other => panic!("expected ProjectCreated, got {other:?}"),
    }
}

#[tokio::test]
async fn task_plan_and_search_lists_default_to_ten_and_cap_limit() {
    let app = test_app().await;
    let actor = Actor::user();
    let project_id = seed_project(&app).await;

    for i in 0..12 {
        let mut task = NewTask::new(format!("needle task {i}"));
        task.project_id = Some(project_id);
        app.state
            .commands
            .dispatch(Command::CreateTask { task }, actor.clone())
            .await
            .unwrap();

        let plan = NewPlan::new(format!("needle plan {i}"), project_id, actor.clone());
        app.state
            .commands
            .dispatch(
                Command::CreatePlan {
                    plan,
                    external_ref: None,
                },
                actor.clone(),
            )
            .await
            .unwrap();
    }

    let (status, tasks) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/tasks?project_id={project_id}&status=all"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tasks response: {tasks}");
    assert_eq!(tasks.as_array().unwrap().len(), 10);

    let (status, tasks) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/tasks?project_id={project_id}&status=all&limit=2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tasks response: {tasks}");
    assert_eq!(tasks.as_array().unwrap().len(), 2);

    let (status, task_page1) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/tasks?project_id={project_id}&status=all&limit=2&page=true"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tasks response: {task_page1}");
    assert_eq!(task_page1["items"].as_array().unwrap().len(), 2);
    assert_eq!(task_page1["has_more"], true);
    let task_cursor = task_page1["next_cursor"].as_str().unwrap();
    let (status, task_page2) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/tasks?project_id={project_id}&status=all&limit=2&cursor={task_cursor}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tasks response: {task_page2}");
    assert_eq!(task_page2["items"].as_array().unwrap().len(), 2);

    let (status, plans) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/plans?project_id={project_id}&status=all"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "plans response: {plans}");
    assert_eq!(plans.as_array().unwrap().len(), 10);

    let (status, plans) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/plans?project_id={project_id}&status=all&limit=0"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "plans response: {plans}");
    assert_eq!(plans.as_array().unwrap().len(), 1);

    let (status, plan_page1) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/plans?project_id={project_id}&status=all&limit=2&page=true"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "plans response: {plan_page1}");
    assert_eq!(plan_page1["items"].as_array().unwrap().len(), 2);
    assert_eq!(plan_page1["has_more"], true);
    assert!(plan_page1["next_cursor"].is_string());

    let (status, hits) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/search?query=needle&scope=tasks,plans&project_id={project_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "search response: {hits}");
    assert_eq!(hits.as_array().unwrap().len(), 10);

    let (status, hit_page1) = json_get(
        app.router,
        &app.admin_token,
        &format!(
            "/v1/search?query=needle&scope=tasks,plans&project_id={project_id}&limit=3&page=true"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "search response: {hit_page1}");
    assert_eq!(hit_page1["items"].as_array().unwrap().len(), 3);
    assert_eq!(hit_page1["has_more"], true);
    assert!(hit_page1["next_cursor"].is_string());
}
