//! E2E coverage for MCP Roadmap M1/M2 read-only endpoints.

use axum::http::StatusCode;
use daruma_core::{Command, CommandBus};
use daruma_domain::{Actor, NewPlan, NewTask, RelationKind, Status};
use daruma_events::Event;
use daruma_shared::{PlanId, ProjectId, TaskId};
use serde_json::json;

mod common;
use common::{json_get, json_post, test_app};

async fn seed_plan(bus: &CommandBus) -> (ProjectId, PlanId, TaskId, TaskId, TaskId) {
    let actor = Actor::user();
    let project_envs = bus
        .dispatch(
            Command::CreateProject {
                title: "Roadmap".to_string(),
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

    let a = TaskId::new();
    let b = TaskId::new();
    let c = TaskId::new();
    for (id, title) in [(a, "A"), (b, "B"), (c, "C")] {
        let mut task = NewTask::new(title);
        task.id = Some(id);
        task.project_id = Some(project_id);
        bus.dispatch(Command::CreateTask { task }, actor.clone())
            .await
            .unwrap();
    }

    let plan_envs = bus
        .dispatch(
            Command::CreatePlan {
                plan: NewPlan::new("MCP Roadmap", project_id, actor.clone()),
                external_ref: None,
            },
            actor.clone(),
        )
        .await
        .unwrap();
    let plan_id = match &plan_envs[0].payload {
        Event::PlanCreated { plan } => plan.id,
        other => panic!("expected PlanCreated, got {other:?}"),
    };

    bus.dispatch(
        Command::AddPlanTask {
            plan_id,
            task_id: a,
            position: Some(0),
            depends_on: None,
        },
        actor.clone(),
    )
    .await
    .unwrap();
    bus.dispatch(
        Command::AddPlanTask {
            plan_id,
            task_id: b,
            position: Some(1),
            depends_on: Some(vec![a]),
        },
        actor.clone(),
    )
    .await
    .unwrap();
    bus.dispatch(
        Command::AddPlanTask {
            plan_id,
            task_id: c,
            position: Some(2),
            depends_on: None,
        },
        actor.clone(),
    )
    .await
    .unwrap();
    bus.dispatch(
        Command::LinkTasks {
            from: c,
            to: b,
            kind: RelationKind::Blocks,
        },
        actor,
    )
    .await
    .unwrap();

    (project_id, plan_id, a, b, c)
}

#[tokio::test]
async fn graph_fanout_and_can_start_respect_depends_on_and_blocks() {
    let app = test_app().await;
    let (_project_id, plan_id, a, b, c) = seed_plan(&app.state.commands).await;

    let (status, graph) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/plans/{plan_id}/graph"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "graph response: {graph}");
    assert_eq!(graph["nodes"].as_array().unwrap().len(), 3);
    assert!(
        graph["edges"]
            .as_array()
            .unwrap()
            .contains(&json!({"from": a, "to": b, "kind": "depends_on"})),
        "graph must include depends_on edge: {graph}"
    );
    assert!(
        graph["edges"]
            .as_array()
            .unwrap()
            .contains(&json!({"from": c, "to": b, "kind": "blocks"})),
        "graph must include blocks edge: {graph}"
    );

    let (status, can_start) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/tasks/{b}/can_start"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "can_start response: {can_start}");
    assert_eq!(can_start["ready"], false);
    assert_eq!(can_start["blockers"][0]["task_id"], json!(c));

    let (status, warned) = json_post(
        app.router.clone(),
        &app.admin_token,
        "/v1/commands",
        &json!({
            "command": {"type": "set_status", "id": b, "status": "in_progress"},
            "actor": {"kind": "user"}
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "set_status response: {warned}");
    assert_eq!(warned["success"], true);
    assert_eq!(warned["warnings"][0]["code"], "task_blocked");
    assert_eq!(
        warned["warnings"][0]["details"]["blockers"][0]["task_id"],
        json!(c)
    );

    let (status, forced) = json_post(
        app.router.clone(),
        &app.admin_token,
        "/v1/commands",
        &json!({
            "command": {"type": "set_status", "id": b, "status": "in_progress", "force": true},
            "actor": {"kind": "user"}
        })
        .to_string(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "forced set_status response: {forced}"
    );
    assert!(
        forced.get("warnings").is_none(),
        "forced response: {forced}"
    );

    let (status, fanout) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/plans/{plan_id}/fanout"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fanout response: {fanout}");
    assert_eq!(
        fanout,
        json!([
            {"wave": 0, "tasks": [a, c]},
            {"wave": 1, "tasks": [b]},
        ])
    );

    app.state
        .commands
        .dispatch(
            Command::SetPlanStatus {
                plan_id,
                status: daruma_domain::PlanStatus::Active,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let (status, drained) = json_post(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/plans/{plan_id}/drain-next"),
        r#"{"claim_ttl_secs":60}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "drain response: {drained}");
    assert_eq!(drained["task_id"], json!(a));
    assert_eq!(drained["claim"]["agent_id"], json!(app.admin_agent_id));

    app.state
        .commands
        .dispatch(
            Command::SetStatus {
                id: c,
                status: Status::Done,
                force: false,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let (_, can_start) = json_get(
        app.router,
        &app.admin_token,
        &format!("/v1/tasks/{b}/can_start"),
    )
    .await;
    assert_eq!(can_start["ready"], true);
    assert_eq!(can_start["blockers"], json!([]));
}
