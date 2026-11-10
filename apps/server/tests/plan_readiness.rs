//! E2E coverage for MCP Roadmap M1/M2 read-only endpoints.

use axum::http::StatusCode;
use daruma_core::{Command, CommandBus};
use daruma_domain::{Actor, NewPlan, NewTask, Priority, RelationKind, Status};
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
        // Todo, not the default inbox: drain only hands out triaged work.
        task.status = Some(daruma_domain::Status::Todo);
        if id == a {
            task.priority = Some(Priority::P1);
        }
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
    let priority_of = |id: TaskId| {
        graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["task_id"] == json!(id))
            .map(|n| n["priority"].clone())
    };
    assert_eq!(
        priority_of(a),
        Some(json!("p1")),
        "graph node carries priority: {graph}"
    );
    assert_eq!(
        priority_of(b),
        Some(json!("p2")),
        "default priority is p2: {graph}"
    );
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

    // An `override_reason` without `force` overrides nothing (the rule engine
    // needs both), so it must not cost the caller the soft warning either —
    // otherwise they get neither the bypass nor the heads-up.
    let (status, with_reason) = json_post(
        app.router.clone(),
        &app.admin_token,
        "/v1/commands",
        &json!({
            "command": {
                "type": "set_status",
                "id": b,
                "status": "in_progress",
                "override_reason": "explaining myself without forcing"
            },
            "actor": {"kind": "user"}
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "set_status response: {with_reason}");
    assert_eq!(
        with_reason["warnings"][0]["code"], "task_blocked",
        "a reason without force must not swallow the warning: {with_reason}"
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
                force: false,
                override_reason: None,
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
                override_reason: None,
                comment: None,
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

#[tokio::test]
async fn selection_explanation_distinguishes_transition_and_graph_preview_without_writes() {
    let app = test_app().await;
    let (project_id, plan_id, a, b, _) = seed_plan(&app.state.commands).await;
    app.state
        .commands
        .dispatch(
            Command::SetPlanStatus {
                plan_id,
                status: daruma_domain::PlanStatus::Active,
                force: false,
                override_reason: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    let claims_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_claims")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    let (_, start) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/tasks/{a}/can_start"),
    )
    .await;
    assert_eq!(start["transition_allowed"], true);
    assert_eq!(start["content_policy"], "not_evaluated");
    assert_eq!(start["plan_membership"][0]["plan_id"], json!(plan_id));
    assert_eq!(start["plan_membership"][0]["selection_candidate"], json!(a));
    assert_eq!(start["selection_context"]["rules"], "not_consulted");
    let (_, blocked) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/tasks/{b}/can_start"),
    )
    .await;
    assert_eq!(blocked["ready"], false);
    assert_eq!(blocked["transition_allowed"], true);
    let (_, ready) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/ready?project_id={project_id}"),
    )
    .await;
    assert_eq!(ready["semantics"], "graph_preview");
    assert_eq!(ready["selection_candidates"][0]["task_id"], json!(a));
    assert_eq!(ready["selection_context"]["rules"], "not_consulted");
    assert_eq!(ready["content_policy"], "not_evaluated");
    let (_, progress) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/plans/{plan_id}/progress"),
    )
    .await;
    assert_eq!(progress["selection_candidate"], json!(a));
    assert_eq!(progress["next_ready"], json!(a));
    assert_eq!(progress["content_policy"], "not_evaluated");
    assert_eq!(progress["selection_context"]["rules"], "not_consulted");
    assert_eq!(progress["selection_observation"]["examined"], 1);
    assert_eq!(
        progress["selection_observation"]["stopped_at_candidate"],
        true
    );
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(before, after);
    let claims_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_claims")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(claims_before, claims_after);
}

#[tokio::test]
async fn selection_explanation_respects_resolver_session_context() {
    use daruma_core::{repos::PlanRepository, NextTaskResolver};
    use daruma_shared::{AgentSessionId, RunId};
    let app = test_app().await;
    let (_, plan_id, a, _, c) = seed_plan(&app.state.commands).await;
    app.state
        .commands
        .dispatch(
            Command::SetPlanStatus {
                plan_id,
                status: daruma_domain::PlanStatus::Active,
                force: false,
                override_reason: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let sibling = AgentSessionId::new();
    let session = AgentSessionId::new();
    let now = chrono::Utc::now().to_rfc3339();
    for id in [sibling, session] {
        sqlx::query("INSERT INTO agent_sessions (id, agent_id, started_at) VALUES (?, ?, ?)")
            .bind(id.to_string())
            .bind(app.admin_agent_id.to_string())
            .bind(&now)
            .execute(&app.pool)
            .await
            .unwrap();
    }
    app.state
        .claims
        .try_acquire_in_session_recorded(
            Actor::user(),
            app.admin_agent_id,
            a,
            chrono::Duration::minutes(5),
            Some(sibling),
        )
        .await
        .unwrap();
    let resolver = NextTaskResolver {
        plans: app.state.plans.as_ref() as &dyn PlanRepository,
        tasks: &app.state.tasks,
        claims: &app.state.claims,
        relations: Some(&app.state.relations),
    };
    let expected = resolver
        .next_for_session(
            plan_id,
            RunId::new(),
            app.admin_agent_id,
            None,
            Some(session),
        )
        .await
        .unwrap()
        .unwrap()
        .task_id;
    assert_eq!(expected, c);
    let (_, progress) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/plans/{plan_id}/progress?session_id={session}"),
    )
    .await;
    assert_eq!(
        progress["selection_candidate"],
        json!(expected),
        "progress: {progress}"
    );
    assert_eq!(progress["next_ready"], json!(expected));
    assert_eq!(progress["selection_context"]["session_id"], json!(session));
    assert_eq!(progress["selection_context"]["agent_id"], json!(app.admin_agent_id));
    assert_eq!(progress["selection_observation"]["sibling_claim"], 1);
    assert_eq!(progress["selection_observation"]["dependencies"], 1);
    assert_eq!(progress["selection_observation"]["examined"], 3);
    let (_, ready) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!(
            "/v1/ready?project_id={}&session_id={session}",
            app.state
                .plans
                .get(plan_id)
                .await
                .unwrap()
                .unwrap()
                .project_id
        ),
    )
    .await;
    assert_eq!(ready["selection_candidates"][0]["task_id"], json!(c));
    let (_, start) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/tasks/{a}/can_start?session_id={session}"),
    )
    .await;
    assert_eq!(start["plan_membership"][0]["selection_candidate"], json!(c));
    assert_eq!(start["transition_scope"], "lifecycle_rules_only");
}

#[tokio::test]
async fn selection_explanation_matches_resolver_across_plan_states() {
    use daruma_core::{repos::PlanRepository, NextTaskResolver};
    use daruma_shared::{AgentId, RunId};
    let app = test_app().await;
    let (_, plan_id, a, b, c) = seed_plan(&app.state.commands).await;
    let actor = Actor::user();
    app.state
        .commands
        .dispatch(
            Command::SetPlanStatus {
                plan_id,
                status: daruma_domain::PlanStatus::Active,
                force: false,
                override_reason: None,
            },
            actor.clone(),
        )
        .await
        .unwrap();
    let resolver = NextTaskResolver {
        plans: app.state.plans.as_ref() as &dyn PlanRepository,
        tasks: &app.state.tasks,
        claims: &app.state.claims,
        relations: Some(&app.state.relations),
    };
    async fn assert_candidate(
        app: &common::TestApp,
        resolver: &NextTaskResolver<'_>,
        plan_id: PlanId,
        expected: Option<TaskId>,
    ) {
        let resolved = resolver
            .next_for_session(plan_id, RunId::new(), app.admin_agent_id, None, None)
            .await
            .unwrap()
            .map(|v| v.task_id);
        assert_eq!(resolved, expected);
        let (status, progress) = json_get(
            app.router.clone(),
            &app.admin_token,
            &format!("/v1/plans/{plan_id}/progress"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(progress["selection_candidate"], json!(expected));
    }
    assert_candidate(&app, &resolver, plan_id, Some(a)).await;
    app.state
        .commands
        .dispatch(
            Command::SetStatus {
                id: a,
                status: Status::Inbox,
                force: false,
                override_reason: None,
                comment: None,
            },
            actor.clone(),
        )
        .await
        .unwrap();
    assert_candidate(&app, &resolver, plan_id, Some(c)).await;
    app.state
        .claims
        .try_acquire_in_session_recorded(
            actor.clone(),
            AgentId::new(),
            c,
            chrono::Duration::minutes(5),
            None,
        )
        .await
        .unwrap();
    assert_candidate(&app, &resolver, plan_id, None).await;
    app.state
        .commands
        .dispatch(
            Command::SetStatus {
                id: a,
                status: Status::Done,
                force: false,
                override_reason: None,
                comment: None,
            },
            actor.clone(),
        )
        .await
        .unwrap();
    app.state
        .commands
        .dispatch(
            Command::SetStatus {
                id: c,
                status: Status::Done,
                force: false,
                override_reason: None,
                comment: None,
            },
            actor.clone(),
        )
        .await
        .unwrap();
    assert!(app
        .state
        .relations
        .list_by_task(b)
        .await
        .unwrap()
        .iter()
        .any(|r| r.kind == RelationKind::WasBlocking));
    assert_candidate(&app, &resolver, plan_id, Some(b)).await;
    // Archiving projects the plan to abandoned, which the resolver skips.
    app.state
        .commands
        .dispatch(Command::ArchivePlan { id: plan_id }, actor.clone())
        .await
        .unwrap();
    assert_candidate(&app, &resolver, plan_id, None).await;
    app.state
        .commands
        .dispatch(
            Command::SetPlanStatus {
                plan_id,
                status: daruma_domain::PlanStatus::Abandoned,
                force: false,
                override_reason: None,
            },
            actor,
        )
        .await
        .unwrap();
    assert_candidate(&app, &resolver, plan_id, None).await;
}

#[tokio::test]
async fn selection_explanation_denies_invisible_blocker_without_leak_or_writes() {
    use daruma_auth::{Capabilities, Capability, ProjectFilter};
    let app = test_app().await;
    let (project, _, a, _, _) = seed_plan(&app.state.commands).await;
    let env = app
        .state
        .commands
        .dispatch(
            Command::CreateProject {
                title: "Private".into(),
                description: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let private = match &env[0].payload {
        Event::ProjectCreated { project } => project.id,
        _ => unreachable!(),
    };
    let hidden = TaskId::new();
    let mut task = NewTask::new("secret blocker");
    task.id = Some(hidden);
    task.project_id = Some(private);
    task.status = Some(Status::Todo);
    app.state
        .commands
        .dispatch(Command::CreateTask { task }, Actor::user())
        .await
        .unwrap();
    app.state
        .commands
        .dispatch(
            Command::LinkTasks {
                from: hidden,
                to: a,
                kind: RelationKind::Blocks,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let caps = Capabilities::from(
        [
            Capability::TaskRead,
            Capability::ProjectRead,
            Capability::PlanRead,
        ]
        .into_iter(),
    );
    let (token, _) = common::mint_pat(
        &app.auth_store(),
        caps,
        ProjectFilter::Only {
            projects: vec![project],
        },
    )
    .await;
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    let claims: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_claims")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    let (status, body) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/tasks/{a}/can_start"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let text = body.to_string();
    assert!(
        !text.contains(&hidden.to_string()) && !text.contains("secret blocker"),
        "{text}"
    );
    assert_eq!(
        before,
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM events")
            .fetch_one(&app.pool)
            .await
            .unwrap()
    );
    assert_eq!(
        claims,
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_claims")
            .fetch_one(&app.pool)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn selection_explanation_reports_real_rule_without_skipping_candidate() {
    let app = common::TestAppBuilder::default()
        .rule_engine_gate()
        .build()
        .await;
    let (project, plan, a, _, _) = seed_plan(&app.state.commands).await;
    app.state
        .commands
        .dispatch(
            Command::SetPlanStatus {
                plan_id: plan,
                status: daruma_domain::PlanStatus::Active,
                force: false,
                override_reason: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let rule = json!({"rule": {"rule_key":"owner-before-start", "title":"Owner required", "scope":{"kind":"project","id":project}, "trigger":"task.before_start", "requirement":{"type":"owner_required"}, "mode":"required"}});
    let (status, body) = json_post(
        app.router.clone(),
        &app.admin_token,
        "/v1/rules",
        &rule.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, progress) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/plans/{plan}/progress"),
    )
    .await;
    let (_, ready) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/ready?project_id={project}"),
    )
    .await;
    let (_, start) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/tasks/{a}/can_start"),
    )
    .await;
    assert_eq!(progress["selection_candidate"], json!(a));
    assert_eq!(ready["selection_candidates"][0]["task_id"], json!(a));
    assert_eq!(start["plan_membership"][0]["selection_candidate"], json!(a));
    assert_eq!(start["selection_context"]["rules"], "not_consulted");
    assert_eq!(start["transition_allowed"], false, "{start}");
    let (status, body) = json_post(
        app.router.clone(),
        &app.admin_token,
        "/v1/commands",
        &json!({"command":{"type":"set_status","id":a,"status":"in_progress"}}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}
