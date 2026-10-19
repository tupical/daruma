//! Token `ProjectFilter` isolation on the read paths an agent and its hooks
//! use: a token scoped to project A never sees project B's tasks, plans or
//! the project itself — neither by listing nor by id — while two tokens
//! scoped to the same project still share its task list.

use axum::http::StatusCode;
use daruma_auth::{Capabilities, Capability, ProjectFilter};
use daruma_shared::ProjectId;
use serde_json::{json, Value};

mod common;
use common::{json_get, json_post, mint_pat, test_app, TestApp};

fn created_id(events: &Value, kind: &str, field: &str) -> String {
    events["data"]
        .as_array()
        .expect("events array")
        .iter()
        .find_map(|event| {
            (event["payload"]["type"] == kind)
                .then(|| event["payload"][field]["id"].as_str().map(str::to_owned))
                .flatten()
        })
        .unwrap_or_else(|| panic!("{kind} event in {events}"))
}

async fn create_project(h: &TestApp, title: &str) -> String {
    let (s, ev) = json_post(
        h.router.clone(),
        &h.admin_token,
        "/v1/commands",
        &json!({"command": {"type": "create_project", "title": title}}).to_string(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "create_project: {ev}");
    created_id(&ev, "project_created", "project")
}

async fn create_task(h: &TestApp, project_id: &str, title: &str) -> String {
    let (s, ev) = json_post(
        h.router.clone(),
        &h.admin_token,
        "/v1/commands",
        &json!({"command": {"type": "create_task",
            "task": {"title": title, "project_id": project_id}}})
        .to_string(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "create_task: {ev}");
    created_id(&ev, "task_created", "task")
}

async fn create_plan(h: &TestApp, project_id: &str, title: &str) -> String {
    let (s, resp) = json_post(
        h.router.clone(),
        &h.admin_token,
        "/v1/plans",
        &json!({"plan": {"project_id": project_id, "title": title, "owner": {"kind": "user"}}})
            .to_string(),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create_plan: {resp}");
    let (s, list) = json_get(
        h.router.clone(),
        &h.admin_token,
        &format!("/v1/plans?project_id={project_id}&status=all"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list plans: {list}");
    list.as_array().expect("plans array")[0]["id"]
        .as_str()
        .expect("plan id")
        .to_owned()
}

async fn token_for(h: &TestApp, project_id: &str) -> String {
    let caps = Capabilities::from(
        [
            Capability::TaskRead,
            Capability::ProjectRead,
            Capability::PlanRead,
        ]
        .into_iter(),
    );
    let only = ProjectFilter::Only {
        projects: vec![project_id.parse::<ProjectId>().expect("project id")],
    };
    mint_pat(&h.auth_store(), caps, only).await.0
}

fn ids(rows: &Value) -> Vec<String> {
    rows.as_array()
        .expect("array")
        .iter()
        .map(|row| row["id"].as_str().expect("id").to_owned())
        .collect()
}

#[tokio::test]
async fn token_scoped_to_one_project_never_sees_another_projects_work() {
    let h = test_app().await;
    let project_a = create_project(&h, "Alpha").await;
    let project_b = create_project(&h, "Beta").await;
    let task_a = create_task(&h, &project_a, "isolation alpha task").await;
    let task_b = create_task(&h, &project_b, "isolation beta task").await;
    let plan_b = create_plan(&h, &project_b, "Beta plan").await;
    let token_a = token_for(&h, &project_a).await;
    let app = || h.router.clone();

    // List without project_id — what a stop hook asks — keeps only project A.
    let (s, tasks) = json_get(app(), &token_a, "/v1/tasks?status=all").await;
    assert_eq!(s, StatusCode::OK, "{tasks}");
    assert_eq!(ids(&tasks), vec![task_a.clone()]);

    let (s, _) = json_get(
        app(),
        &token_a,
        &format!("/v1/tasks?status=all&project_id={project_b}"),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "foreign project listing");

    let (s, _) = json_get(app(), &token_a, &format!("/v1/tasks/{task_b}")).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "foreign task by id");
    let (s, task) = json_get(app(), &token_a, &format!("/v1/tasks/{task_a}")).await;
    assert_eq!(s, StatusCode::OK, "own task by id: {task}");

    let (s, hits) = json_get(app(), &token_a, "/v1/search?query=isolation&scope=tasks").await;
    assert_eq!(s, StatusCode::OK, "{hits}");
    let hit_tasks: Vec<String> = hits
        .as_array()
        .expect("hits array")
        .iter()
        .map(|hit| hit["task_id"].as_str().expect("task_id").to_owned())
        .collect();
    assert_eq!(
        hit_tasks,
        vec![task_a.clone()],
        "search keeps own hits only"
    );

    let (s, projects) = json_get(app(), &token_a, "/v1/projects").await;
    assert_eq!(s, StatusCode::OK, "{projects}");
    assert_eq!(ids(&projects), vec![project_a.clone()]);

    let (s, _) = json_get(
        app(),
        &token_a,
        &format!("/v1/plans?project_id={project_b}&status=all"),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "foreign plan listing");
    let (s, _) = json_get(app(), &token_a, &format!("/v1/plans/{plan_b}")).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "foreign plan by id");
    let (s, _) = json_get(app(), &token_a, &format!("/v1/plans/{plan_b}/progress")).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "foreign plan progress");
    let (s, _) = json_get(
        app(),
        &token_a,
        &format!("/v1/ready?project_id={project_b}"),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "foreign project ready queue");
}

#[tokio::test]
async fn tokens_scoped_to_the_same_project_share_its_task_list() {
    let h = test_app().await;
    let shared = create_project(&h, "Shared").await;
    let other = create_project(&h, "Other").await;
    let first = create_task(&h, &shared, "shared one").await;
    let second = create_task(&h, &shared, "shared two").await;
    create_task(&h, &other, "not shared").await;
    let alice = token_for(&h, &shared).await;
    let bob = token_for(&h, &shared).await;

    for token in [&alice, &bob] {
        let (s, tasks) = json_get(h.router.clone(), token, "/v1/tasks?status=all").await;
        assert_eq!(s, StatusCode::OK, "{tasks}");
        assert_eq!(ids(&tasks), vec![first.clone(), second.clone()]);
    }
    // A workspace-wide token is unaffected.
    let (s, tasks) = json_get(h.router.clone(), &h.admin_token, "/v1/tasks?status=all").await;
    assert_eq!(s, StatusCode::OK, "{tasks}");
    assert_eq!(ids(&tasks).len(), 3);
}
