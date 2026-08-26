//! HTTP API tests for lifecycle rules and evidence (docs/LIFECYCLE_RULES_SPEC.md):
//! write/read round-trips through `/v1/rules` and `/v1/evidence`.

mod common;

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
    Router,
};
use common::{json_get, json_post, TestAppBuilder};
use daruma_core::Command;
use daruma_domain::{
    Actor, ActorRef, EvidenceKind, NewEvidence, NewPlan, NewRule, NewTask, Requirement, RuleMode,
    RuleScope, RuleTrigger,
};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{EvidenceId, PlanId, ProjectId, RuleId, TaskId};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn json_method(
    app: Router,
    method: Method,
    token: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_owned()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn rule_crud_roundtrip() {
    let app = TestAppBuilder::default().build().await;
    let token = app.admin_token.clone();

    // Create (example 3: completion-note at tenant scope).
    let body = json!({
        "rule": {
            "rule_key": "completion-note",
            "title": "Require who/when/why on completion",
            "scope": { "kind": "tenant" },
            "trigger": "task.before_complete",
            "requirement": {
                "type": "completion_note",
                "required_fields": ["actor", "completed_at", "reason"]
            },
            "mode": "required",
            "message": "Задачу нельзя завершить без отметки кто/когда/зачем.",
            "override_allowed": true
        }
    })
    .to_string();
    let (status, created) = json_post(app.router.clone(), &token, "/v1/rules", &body).await;
    assert_eq!(status, StatusCode::OK, "create: {created}");
    assert_eq!(created["success"], json!(true));
    let rule_id = created["data"]["rule"]["id"]
        .as_str()
        .expect("created rule id")
        .to_string();
    assert_eq!(created["data"]["rule"]["mode"], json!("required"));

    // Get by id.
    let (status, got) = json_get(app.router.clone(), &token, &format!("/v1/rules/{rule_id}")).await;
    assert_eq!(status, StatusCode::OK, "get: {got}");
    assert_eq!(got["rule"]["rule_key"], json!("completion-note"));

    // List at tenant scope.
    let (status, list) = json_get(app.router.clone(), &token, "/v1/rules").await;
    assert_eq!(status, StatusCode::OK, "list: {list}");
    assert_eq!(list["rules"].as_array().unwrap().len(), 1);

    // Duplicate rule_key at the same scope is rejected.
    let (status, _) = json_post(app.router.clone(), &token, "/v1/rules", &body).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "duplicate rule_key at one scope must conflict"
    );

    // Patch: weaken to recommendation.
    let patch = json!({ "mode": "recommendation" }).to_string();
    let (status, patched) = json_method(
        app.router.clone(),
        Method::PATCH,
        &token,
        &format!("/v1/rules/{rule_id}"),
        Some(&patch),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch: {patched}");
    assert_eq!(patched["data"]["rule"]["mode"], json!("recommendation"));

    // Delete permanently. The event remains in history, but the active
    // projection no longer returns the rule.
    let (status, deleted) = json_method(
        app.router.clone(),
        Method::DELETE,
        &token,
        &format!("/v1/rules/{rule_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "delete: {deleted}");

    let (status, _) = json_get(app.router.clone(), &token, &format!("/v1/rules/{rule_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, list) = json_get(app.router.clone(), &token, "/v1/rules").await;
    assert!(list["rules"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn create_rule_validation_rejects_empty_key() {
    let app = TestAppBuilder::default().build().await;
    let token = app.admin_token.clone();
    let body = json!({
        "rule": {
            "rule_key": "",
            "title": "x",
            "scope": { "kind": "tenant" },
            "trigger": "task.before_complete",
            "requirement": { "type": "owner_required" },
            "mode": "required"
        }
    })
    .to_string();
    let (status, _) = json_post(app.router.clone(), &token, "/v1/rules", &body).await;
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "empty rule_key must be rejected, got {status}"
    );
}

async fn seed_scoped_entities(app: &common::TestApp) -> (ProjectId, PlanId, TaskId) {
    let actor = Actor::user();
    let project = app
        .state
        .commands
        .dispatch(
            Command::CreateProject {
                title: "Rule scopes".into(),
                description: None,
            },
            actor.clone(),
        )
        .await
        .unwrap()
        .into_iter()
        .find_map(|env| match env.payload {
            Event::ProjectCreated { project } => Some(project.id),
            _ => None,
        })
        .unwrap();

    let task_id = TaskId::new();
    let mut task = NewTask::new("Scoped task");
    task.id = Some(task_id);
    task.project_id = Some(project);
    app.state
        .commands
        .dispatch(Command::CreateTask { task }, actor.clone())
        .await
        .unwrap();

    let plan = app
        .state
        .commands
        .dispatch(
            Command::CreatePlan {
                plan: NewPlan::new("Scoped plan", project, actor.clone()),
                external_ref: None,
            },
            actor,
        )
        .await
        .unwrap()
        .into_iter()
        .find_map(|env| match env.payload {
            Event::PlanCreated { plan } => Some(plan.id),
            _ => None,
        })
        .unwrap();

    (project, plan, task_id)
}

fn scoped_rule_body(rule_key: &str, kind: &str, id: impl ToString) -> String {
    json!({
        "rule": {
            "rule_key": rule_key,
            "title": "Scoped rule",
            "scope": { "kind": kind, "id": id.to_string() },
            "trigger": "task.before_complete",
            "requirement": { "type": "owner_required" },
            "mode": "required"
        }
    })
    .to_string()
}

#[tokio::test]
async fn create_rule_rejects_missing_scope_targets_with_their_ids() {
    let app = TestAppBuilder::default().build().await;
    let token = app.admin_token.clone();
    let project = ProjectId::new();
    let plan = PlanId::new();
    let task = TaskId::new();
    let missing = [
        (
            "missing-project",
            "project",
            project.as_uuid().to_string(),
            project.to_string(),
        ),
        (
            "missing-plan",
            "plan",
            plan.as_uuid().to_string(),
            plan.to_string(),
        ),
        (
            "missing-task",
            "task",
            task.as_uuid().to_string(),
            task.to_string(),
        ),
    ];

    for (rule_key, kind, wire_id, display_id) in missing {
        let body = scoped_rule_body(rule_key, kind, &wire_id);
        let (status, response) = json_post(app.router.clone(), &token, "/v1/rules", &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{kind}: {response}");
        assert_eq!(response["error"]["code"], "validation");
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains(&display_id),
            "{kind} validation must identify {display_id}: {response}"
        );
    }
}

#[tokio::test]
async fn create_rule_accepts_existing_scope_targets() {
    let app = TestAppBuilder::default().build().await;
    let token = app.admin_token.clone();
    let (project, plan, task) = seed_scoped_entities(&app).await;

    for (rule_key, kind, id) in [
        ("existing-project", "project", project.as_uuid().to_string()),
        ("existing-plan", "plan", plan.as_uuid().to_string()),
        ("existing-task", "task", task.as_uuid().to_string()),
    ] {
        let body = scoped_rule_body(rule_key, kind, &id);
        let (status, response) = json_post(app.router.clone(), &token, "/v1/rules", &body).await;
        assert_eq!(status, StatusCode::OK, "{kind}: {response}");
        assert_eq!(response["data"]["rule"]["scope"]["id"], id);
    }
}

#[tokio::test]
async fn get_rule_still_reads_a_legacy_dangling_scope() {
    let app = TestAppBuilder::default().build().await;
    let missing_project = ProjectId::new();
    let rule_id = RuleId::new();
    let rule = NewRule {
        id: Some(rule_id),
        rule_key: "legacy-dangling".into(),
        title: "Legacy dangling rule".into(),
        scope: RuleScope::Project {
            id: missing_project,
        },
        trigger: RuleTrigger::TaskBeforeComplete,
        condition: None,
        requirement: Requirement::OwnerRequired,
        mode: RuleMode::Required,
        message: String::new(),
        override_allowed: false,
        enabled: true,
    }
    .into_rule(daruma_shared::time::now());
    app.state
        .rules
        .apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::RuleCreated { rule },
        ))
        .await
        .unwrap();

    let (status, response) = json_get(
        app.router,
        &app.admin_token,
        &format!("/v1/rules/{rule_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(
        response["rule"]["scope"]["id"],
        missing_project.as_uuid().to_string()
    );
}

fn scoped_evidence_body(kind: &str, id: impl ToString) -> String {
    json!({
        "evidence": {
            "kind": "completion_note",
            "scope": { "kind": kind, "id": id.to_string() },
            "reason": "done"
        }
    })
    .to_string()
}

#[tokio::test]
async fn record_evidence_rejects_missing_scope_targets_with_their_ids() {
    let app = TestAppBuilder::default().build().await;
    let token = app.admin_token.clone();
    let project = ProjectId::new();
    let plan = PlanId::new();
    let task = TaskId::new();

    for (kind, wire_id, display_id) in [
        (
            "project",
            project.as_uuid().to_string(),
            project.to_string(),
        ),
        ("plan", plan.as_uuid().to_string(), plan.to_string()),
        ("task", task.as_uuid().to_string(), task.to_string()),
    ] {
        let body = scoped_evidence_body(kind, wire_id);
        let (status, response) = json_post(app.router.clone(), &token, "/v1/evidence", &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{kind}: {response}");
        assert_eq!(response["error"]["code"], "validation");
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains(&display_id),
            "{kind} validation must identify {display_id}: {response}"
        );
    }
}

#[tokio::test]
async fn record_evidence_accepts_existing_scope_targets() {
    let app = TestAppBuilder::default().build().await;
    let token = app.admin_token.clone();
    let (project, plan, task) = seed_scoped_entities(&app).await;

    for (kind, id) in [
        ("project", project.as_uuid().to_string()),
        ("plan", plan.as_uuid().to_string()),
        ("task", task.as_uuid().to_string()),
    ] {
        let body = scoped_evidence_body(kind, &id);
        let (status, response) = json_post(app.router.clone(), &token, "/v1/evidence", &body).await;
        assert_eq!(status, StatusCode::OK, "{kind}: {response}");
        assert_eq!(response["data"]["evidence"]["scope"]["id"], id);
    }
}

#[tokio::test]
async fn get_evidence_still_reads_a_legacy_dangling_scope() {
    let app = TestAppBuilder::default().build().await;
    let missing_project = ProjectId::new();
    let evidence_id = EvidenceId::new();
    let evidence = NewEvidence {
        id: Some(evidence_id),
        kind: EvidenceKind::CompletionNote,
        scope: RuleScope::Project {
            id: missing_project,
        },
        target: None,
        doc_version: None,
        reason: "legacy evidence".into(),
        payload: Value::Null,
        project_id: None,
        plan_id: None,
        task_id: None,
        run_id: None,
        artifact_id: None,
        rule_id: None,
        supersedes: None,
    }
    .into_evidence(
        ActorRef::from_actor(&Actor::user()),
        daruma_shared::time::now(),
    );
    app.state
        .evidence
        .apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::EvidenceRecorded { evidence },
        ))
        .await
        .unwrap();

    let (status, response) = json_get(
        app.router,
        &app.admin_token,
        &format!("/v1/evidence/{evidence_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(
        response["evidence"]["scope"]["id"],
        missing_project.as_uuid().to_string()
    );
}
