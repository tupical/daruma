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

#[tokio::test]
async fn scoped_writer_cannot_read_or_overwrite_foreign_comments_or_replay_global_data() {
    let h = test_app().await;
    let a = create_project(&h, "A").await;
    let b = create_project(&h, "B").await;
    let ta = create_task(&h, &a, "own").await;
    let tb = create_task(&h, &b, "foreign").await;
    let token = mint_pat(
        &h.auth_store(),
        Capabilities::from([Capability::Admin]),
        ProjectFilter::Only {
            projects: vec![a.parse().unwrap()],
        },
    )
    .await
    .0;
    // Use explicit statements instead of reusing closures borrowing JSON buffers.
    let (status, added) = json_post(h.router.clone(), &h.admin_token, "/v1/commands", &json!({"command": {"type":"add_comment", "comment":{"task_id":tb,"body":"B confidential"}}}).to_string()).await;
    assert_eq!(status, StatusCode::OK, "{added}");
    let comment = created_id(&added, "comment_added", "comment");
    for path in [
        format!("/v1/tasks/{tb}/comments"),
        "/v1/events".into(),
        "/v1/events/snapshot".into(),
        "/v1/history".into(),
        "/v1/audit/findings".into(),
    ] {
        let (status, body) = json_get(h.router.clone(), &token, &path).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
        assert!(!body.to_string().contains("B confidential"));
    }
    for command in [
        json!({"type":"add_comment","comment":{"id":comment,"task_id":ta,"body":"overwrite"}}),
        json!({"type":"update_task","id":tb,"patch":{"project_id":a}}),
        json!({"type":"link_tasks","from":ta,"to":tb,"kind":"blocks"}),
    ] {
        let (status, body) = json_post(
            h.router.clone(),
            &token,
            "/v1/commands",
            &json!({"command":command}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    let (status, body) = json_get(
        h.router.clone(),
        &h.admin_token,
        &format!("/v1/tasks/{tb}/comments"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.to_string().contains("B confidential"));
    let (status, body) = json_post(
        h.router.clone(),
        &token,
        &format!("/v1/tasks/{ta}/comments"),
        &json!({"body":"own comment"}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn scope_filter_precedes_pagination_and_external_key_normalization() {
    let h = test_app().await;
    let a = create_project(&h, "A").await;
    let b = create_project(&h, "B").await;
    let (s, body) = json_post(h.router.clone(), &h.admin_token, "/v1/commands", &json!({"command":{"type":"create_task","task":{"title":"private","project_id":b,"external_key":"foreign-key"}}}).to_string()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let ta = create_task(&h, &a, "visible").await;
    let token = mint_pat(
        &h.auth_store(),
        Capabilities::from([Capability::Admin]),
        ProjectFilter::Only {
            projects: vec![a.parse().unwrap()],
        },
    )
    .await
    .0;
    let (s, page) = json_get(
        h.router.clone(),
        &token,
        "/v1/tasks?status=all&page=true&limit=1",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{page}");
    assert_eq!(page["items"][0]["id"], ta);
    assert_eq!(page["has_more"], false);
    let (s, body) = json_post(h.router.clone(), &token, "/v1/commands", &json!({"command":{"type":"create_task","task":{"title":"attack","project_id":a,"external_key":" foreign-key "}}}).to_string()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
}

#[tokio::test]
async fn scoped_plan_cannot_reuse_foreign_source_metadata() {
    let h = test_app().await;
    let a = create_project(&h, "A").await;
    let b = create_project(&h, "B").await;
    let (s, body) = json_post(h.router.clone(), &h.admin_token, "/v1/commands", &json!({"command":{"type":"materialize_plan","plan":{"project_id":b,"title":"B source","owner":{"kind":"user"},"source":{"ref":"https://example.com/private","label":"B secret source"}},"tasks":[{"title":"source task"}]}}).to_string()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let token = mint_pat(
        &h.auth_store(),
        Capabilities::from([Capability::Admin]),
        ProjectFilter::Only {
            projects: vec![a.parse().unwrap()],
        },
    )
    .await
    .0;
    let (s, body) = json_post(h.router.clone(), &token, "/v1/commands", &json!({"command":{"type":"materialize_plan","plan":{"project_id":a,"title":"A source","owner":{"kind":"user"},"source":{"ref":"https://example.com/private","note":"fill missing field"}},"tasks":[{"title":"source task"}]}}).to_string()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    assert!(!body.to_string().contains("B secret source"));
}

#[tokio::test]
async fn scoped_normal_plan_lifecycle_remains_available() {
    let h = test_app().await;
    let a = create_project(&h, "A").await;
    let token = mint_pat(
        &h.auth_store(),
        Capabilities::from([Capability::Admin]),
        ProjectFilter::Only {
            projects: vec![a.parse().unwrap()],
        },
    )
    .await
    .0;
    let (s, body) = json_post(h.router.clone(), &token, "/v1/commands", &json!({"command":{"type":"materialize_plan","plan":{"project_id":a,"title":"Scoped work","owner":{"kind":"user"},"source":{"label":"Scoped request"}},"tasks":[{"title":"Implement","status":"todo"}]}}).to_string()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let plan = created_id(&body, "plan_created", "plan");
    let task = created_id(&body, "task_created", "task");
    let (s, body) = json_post(
        h.router.clone(),
        &token,
        &format!("/v1/plans/{plan}/status"),
        &json!({"status":"active"}).to_string(),
    )
    .await;
    assert!(s.is_success(), "{body}");
    let (s, body) = json_post(
        h.router.clone(),
        &token,
        &format!("/v1/plans/{plan}/drain-next"),
        "{}",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["task_id"], task);
    for command in [
        json!({"type":"set_status","id":task,"status":"in_progress"}),
        json!({"type":"complete_task","id":task,"note":{"reason":"verified"}}),
    ] {
        let (s, body) = json_post(
            h.router.clone(),
            &token,
            "/v1/commands",
            &json!({"command":command}).to_string(),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
    }
    let (s, body) = json_get(h.router.clone(), &token, &format!("/v1/tasks/{task}")).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "done");
}

#[tokio::test]
async fn plan_collection_does_not_expand_a_foreign_shared_source() {
    let h = test_app().await;
    let a = create_project(&h, "A").await;
    let b = create_project(&h, "B").await;
    for (project, title) in [(&b, "B hidden source"), (&a, "A plan")] {
        let (s, body) = json_post(h.router.clone(), &h.admin_token, "/v1/commands", &json!({"command":{"type":"materialize_plan","plan":{"project_id":project,"title":title,"owner":{"kind":"user"},"source":{"ref":"https://example.com/shared-source","label":title}},"tasks":[{"title":"work"}]}}).to_string()).await;
        assert_eq!(s, StatusCode::OK, "{body}");
    }
    let token = token_for(&h, &a).await;
    let (s, body) = json_get(
        h.router.clone(),
        &token,
        &format!("/v1/plans?project_id={a}&status=all&source_chain=true"),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    assert!(!body.to_string().contains("B hidden source"));
}

#[tokio::test]
async fn run_timeline_cannot_reveal_a_foreign_step_in_an_allowed_plan() {
    let h = test_app().await;
    let a = create_project(&h, "A").await;
    let b = create_project(&h, "B").await;
    let foreign = create_task(&h, &b, "B private task").await;
    let plan = create_plan(&h, &a, "A plan").await;
    let (s, body) = json_post(
        h.router.clone(),
        &h.admin_token,
        &format!("/v1/plans/{plan}/status"),
        &json!({"status":"active"}).to_string(),
    )
    .await;
    assert!(s.is_success(), "{body}");
    let agent = daruma_shared::AgentId::new();
    let (s, body) = json_post(
        h.router.clone(),
        &h.admin_token,
        "/v1/runs",
        &json!({"plan_id":plan,"agent_id":agent}).to_string(),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let run = body["data"]["run_id"].as_str().unwrap().to_owned();
    let (s, body) = json_post(
        h.router.clone(),
        &h.admin_token,
        &format!("/v1/runs/{run}/step/start"),
        &json!({"task_id":foreign}).to_string(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let token = mint_pat(
        &h.auth_store(),
        Capabilities::from([Capability::Admin]),
        ProjectFilter::Only {
            projects: vec![a.parse().unwrap()],
        },
    )
    .await
    .0;
    for path in [
        format!("/v1/runs/{run}/timeline"),
        format!("/v1/plans/{plan}/runs"),
    ] {
        let (s, body) = json_get(h.router.clone(), &token, &path).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{path}: {body}");
        assert!(!body.to_string().contains(&foreign));
    }
}
