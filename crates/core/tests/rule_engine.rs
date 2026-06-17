//! Rule-engine integration tests (docs/LIFECYCLE_RULES_SPEC.md §5).
//!
//! Wires the real `RuleRepo` + `RuleEngineGate` into a `CommandHandler` and
//! exercises the three example rules from the spec:
//!   1. read-architecture-md  (read_artifact, required)   → blocks plan approve
//!   2. auth-impact-check      (impact_check, required)    → blocks task start
//!   3. completion-note        (completion_note, required) → blocks task complete
//!
//! Also covers the mode matrix (off / recommendation / required) and the
//! determinism / override semantics. Decisions are deterministic: the same
//! rules + transition always yield the same result.

use std::sync::Arc;

use taskagent_core::rule_engine::RuleEngineGate;
use taskagent_core::{Command, CommandHandler};
use taskagent_domain::{
    Actor, NewPlan, NewRule, PlanStatus, Requirement, Rule, RuleMode, RuleScope, RuleTrigger,
    Status,
};
use taskagent_events::{Event, EventBus, EventStore};
use taskagent_shared::{CoreError, PlanId, ProjectId, TaskId};
use taskagent_storage::{
    ActivityRepo, CommentRepo, Db, PlanRepo, ProjectRepo, RuleRepo, SqliteEventStore, TaskRepo,
};

struct Stack {
    handler: CommandHandler,
}

async fn stack() -> Stack {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let tasks = Arc::new(TaskRepo::new(pool.clone()));
    let projects = Arc::new(ProjectRepo::new(pool.clone()));
    let comments = Arc::new(CommentRepo::new(pool.clone()));
    let activity = Arc::new(ActivityRepo::new(pool.clone()));
    let plans = Arc::new(PlanRepo::new(pool.clone()));
    let rules = Arc::new(RuleRepo::new(pool.clone()));

    let handler = CommandHandler::new(
        store,
        tasks,
        projects,
        comments,
        activity,
        EventBus::default(),
    )
    .with_plans(plans)
    .with_rules(rules.clone())
    .with_lifecycle_gate(Arc::new(RuleEngineGate::new(rules.clone())));

    Stack { handler }
}

fn new_rule(
    rule_key: &str,
    scope: RuleScope,
    trigger: RuleTrigger,
    requirement: Requirement,
    mode: RuleMode,
    override_allowed: bool,
) -> NewRule {
    NewRule {
        id: None,
        rule_key: rule_key.into(),
        title: rule_key.into(),
        scope,
        trigger,
        condition: None,
        requirement,
        mode,
        message: format!("{rule_key} message"),
        override_allowed,
        enabled: true,
    }
}

async fn install(stack: &Stack, rule: NewRule) -> Rule {
    let envs = stack
        .handler
        .handle(Command::CreateRule { rule }, Actor::user())
        .await
        .expect("create rule");
    match &envs[0].payload {
        Event::RuleCreated { rule } => rule.clone(),
        other => panic!("expected RuleCreated, got {other:?}"),
    }
}

async fn create_task(stack: &Stack, title: &str) -> TaskId {
    let envs = stack
        .handler
        .handle(
            Command::CreateTask {
                task: taskagent_domain::NewTask::new(title),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    match &envs[0].payload {
        Event::TaskCreated { task } => task.id.unwrap(),
        other => panic!("expected TaskCreated, got {other:?}"),
    }
}

async fn create_plan(stack: &Stack, project: ProjectId) -> PlanId {
    let new_plan = NewPlan::new("Plan", project, Actor::user());
    let envs = stack
        .handler
        .handle(
            Command::CreatePlan {
                plan: new_plan,
                external_ref: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    match &envs[0].payload {
        Event::PlanCreated { plan } => plan.id,
        other => panic!("expected PlanCreated, got {other:?}"),
    }
}

fn is_blocked(err: &CoreError, fragment: &str) -> bool {
    let msg = err.to_string();
    msg.contains("rule_blocked") && msg.contains(fragment)
}

// ── Example 3: completion-note blocks task.before_complete ──────────────────────

#[tokio::test]
async fn example3_completion_note_required_blocks_complete() {
    let stack = stack().await;
    install(
        &stack,
        new_rule(
            "completion-note",
            RuleScope::Tenant,
            RuleTrigger::TaskBeforeComplete,
            Requirement::CompletionNote {
                required_fields: vec!["actor".into(), "reason".into()],
            },
            RuleMode::Required,
            true,
        ),
    )
    .await;

    let task = create_task(&stack, "Ship it").await;
    let err = stack
        .handler
        .handle(
            Command::SetStatus {
                id: task,
                status: Status::Done,
                force: false,
            },
            Actor::user(),
        )
        .await
        .expect_err("completion-note required must block");
    assert!(is_blocked(&err, "completion-note message"), "got: {err}");

    // The task did not transition (blocked before persist).
    assert_eq!(
        stack.handler.tasks.get(task).await.unwrap().unwrap().status,
        Status::Inbox,
        "blocked before persist — task unchanged"
    );
}

#[tokio::test]
async fn example3_recommendation_warns_but_proceeds() {
    let stack = stack().await;
    install(
        &stack,
        new_rule(
            "completion-note",
            RuleScope::Tenant,
            RuleTrigger::TaskBeforeComplete,
            Requirement::CompletionNote {
                required_fields: vec![],
            },
            RuleMode::Recommendation,
            true,
        ),
    )
    .await;

    let task = create_task(&stack, "Ship it").await;
    let outcome = stack
        .handler
        .handle_with_warnings(
            Command::SetStatus {
                id: task,
                status: Status::Done,
                force: false,
            },
            Actor::user(),
        )
        .await
        .expect("recommendation must not block");
    assert_eq!(outcome.warnings.len(), 1, "one rule warning surfaced");
    assert_eq!(outcome.warnings[0].code, "rule_warning:completion-note");
    assert_eq!(
        stack.handler.tasks.get(task).await.unwrap().unwrap().status,
        Status::Done
    );
}

#[tokio::test]
async fn off_mode_not_evaluated() {
    let stack = stack().await;
    install(
        &stack,
        new_rule(
            "completion-note",
            RuleScope::Tenant,
            RuleTrigger::TaskBeforeComplete,
            Requirement::CompletionNote {
                required_fields: vec![],
            },
            RuleMode::Off,
            true,
        ),
    )
    .await;

    let task = create_task(&stack, "Ship it").await;
    let outcome = stack
        .handler
        .handle_with_warnings(
            Command::SetStatus {
                id: task,
                status: Status::Done,
                force: false,
            },
            Actor::user(),
        )
        .await
        .expect("off rule is inert");
    assert!(outcome.warnings.is_empty(), "off → no warning");
    assert_eq!(
        stack.handler.tasks.get(task).await.unwrap().unwrap().status,
        Status::Done
    );
}

// ── Example 2: auth-impact-check blocks task.before_start ───────────────────────

#[tokio::test]
async fn example2_impact_check_required_blocks_start() {
    let stack = stack().await;
    install(
        &stack,
        new_rule(
            "auth-impact-check",
            RuleScope::Tenant,
            RuleTrigger::TaskBeforeStart,
            Requirement::ImpactCheck {
                target: "auth-module".into(),
                required_fields: vec!["risk_level".into()],
            },
            RuleMode::Required,
            true,
        ),
    )
    .await;

    let task = create_task(&stack, "Touch auth").await;
    let err = stack
        .handler
        .handle(
            Command::SetStatus {
                id: task,
                status: Status::InProgress,
                force: false,
            },
            Actor::user(),
        )
        .await
        .expect_err("impact check required must block start");
    assert!(is_blocked(&err, "auth-impact-check message"), "got: {err}");
    assert_eq!(
        stack.handler.tasks.get(task).await.unwrap().unwrap().status,
        Status::Inbox,
        "blocked before persist — task unchanged"
    );
}

// ── Example 1: read-architecture-md blocks plan.before_approve ──────────────────

#[tokio::test]
async fn example1_read_artifact_required_blocks_plan_approve() {
    let stack = stack().await;
    let project = ProjectId::new();
    install(
        &stack,
        new_rule(
            "read-architecture-md",
            RuleScope::Tenant,
            RuleTrigger::PlanBeforeApprove,
            Requirement::ReadArtifact {
                doc_ref: "architecture.md".into(),
                min_version: "latest".into(),
            },
            RuleMode::Required,
            false,
        ),
    )
    .await;

    let plan = create_plan(&stack, project).await;
    let err = stack
        .handler
        .handle(
            Command::SetPlanStatus {
                plan_id: plan,
                status: PlanStatus::Active,
            },
            Actor::user(),
        )
        .await
        .expect_err("read_artifact required must block approve");
    assert!(
        is_blocked(&err, "read-architecture-md message"),
        "got: {err}"
    );
}

// ── Override (spec §1.5) ────────────────────────────────────────────────────────

#[tokio::test]
async fn override_allowed_rule_passes_with_force_in_commands_path() {
    // The HTTP /commands path is where override_reason rides; the gate honours
    // force only with a non-empty reason. Here force without reason still
    // blocks (silent force does not bypass a required rule — spec §1.5).
    let stack = stack().await;
    install(
        &stack,
        new_rule(
            "completion-note",
            RuleScope::Tenant,
            RuleTrigger::TaskBeforeComplete,
            Requirement::CompletionNote {
                required_fields: vec![],
            },
            RuleMode::Required,
            true,
        ),
    )
    .await;

    let task = create_task(&stack, "Ship it").await;
    let err = stack
        .handler
        .handle(
            Command::SetStatus {
                id: task,
                status: Status::Done,
                force: true, // force alone, no override_reason
            },
            Actor::user(),
        )
        .await
        .expect_err("silent force must not bypass a required rule");
    assert!(is_blocked(&err, "completion-note"), "got: {err}");
}

// ── Determinism (spec invariant 8) ──────────────────────────────────────────────

#[tokio::test]
async fn decision_is_deterministic() {
    let stack = stack().await;
    install(
        &stack,
        new_rule(
            "completion-note",
            RuleScope::Tenant,
            RuleTrigger::TaskBeforeComplete,
            Requirement::CompletionNote {
                required_fields: vec![],
            },
            RuleMode::Required,
            true,
        ),
    )
    .await;

    for _ in 0..3 {
        let task = create_task(&stack, "repeat").await;
        let err = stack
            .handler
            .handle(
                Command::SetStatus {
                    id: task,
                    status: Status::Done,
                    force: false,
                },
                Actor::user(),
            )
            .await
            .expect_err("same inputs, same block");
        assert!(is_blocked(&err, "completion-note"));
    }
}
