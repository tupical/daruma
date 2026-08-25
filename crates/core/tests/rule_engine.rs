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

use daruma_core::lifecycle_gate::{
    GateCheck, GateDecision, GateOverride, LifecycleGate, TriggerEvent,
};
use daruma_core::rule_engine::RuleEngineGate;
use daruma_core::{Command, CommandHandler};
use daruma_domain::{
    Actor, CanStart, Condition, EvidenceKind, NewEvidence, NewPlan, NewRule, PlanStatus,
    Requirement, Rule, RuleMode, RuleScope, RuleTrigger, Status,
};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::{CoreError, PlanId, ProjectId, TaskId};
use daruma_storage::{
    ActivityRepo, CommentRepo, Db, EvidenceRepo, PlanRepo, ProjectRepo, RelationRepo, RuleRepo,
    SqliteEventStore, TaskRepo,
};

struct Stack {
    handler: CommandHandler,
    relations: Arc<RelationRepo>,
    gate: Arc<dyn LifecycleGate>,
}

impl Stack {
    /// `can_start` as the HTTP route calls it: same gate the command dispatch
    /// uses, so the two answers are comparable.
    async fn can_start(&self, task: TaskId) -> CanStart {
        let actor = Actor::user();
        daruma_core::can_start(
            &self.handler.tasks,
            &self.relations,
            Some((self.gate.as_ref(), &actor)),
            task,
        )
        .await
        .expect("can_start")
    }
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
    let evidence = Arc::new(EvidenceRepo::new(pool.clone()));
    let relations = Arc::new(RelationRepo::new(pool.clone()));
    let gate: Arc<dyn LifecycleGate> = Arc::new(RuleEngineGate::with_evidence(
        rules.clone(),
        evidence.clone(),
    ));

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
    .with_evidence(evidence.clone())
    // The gate reads evidence so a satisfied `required` requirement unblocks.
    // The same instance answers `can_start`: two gates could drift, one cannot.
    .with_lifecycle_gate(gate.clone());

    Stack {
        handler,
        relations,
        gate,
    }
}

/// Record a piece of evidence through the command bus (so it lands in the same
/// projection the gate reads).
async fn record_evidence(stack: &Stack, evidence: NewEvidence) {
    stack
        .handler
        .handle(Command::RecordEvidence { evidence }, Actor::user())
        .await
        .expect("record evidence");
}

fn new_evidence(kind: EvidenceKind, scope: RuleScope, target: Option<&str>) -> NewEvidence {
    NewEvidence {
        id: None,
        kind,
        scope,
        target: target.map(|s| s.to_string()),
        doc_version: None,
        reason: "test evidence".into(),
        payload: serde_json::Value::Null,
        project_id: None,
        plan_id: None,
        task_id: None,
        run_id: None,
        artifact_id: None,
        rule_id: None,
        supersedes: None,
    }
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
                task: daruma_domain::NewTask::new(title),
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
                override_reason: None,
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
                override_reason: None,
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
                override_reason: None,
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
                override_reason: None,
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
                force: false,
                override_reason: None,
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

#[tokio::test]
async fn plan_approve_override_passes_and_returns_the_bypassed_rule_as_a_warning() {
    let stack = stack().await;
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
            true,
        ),
    )
    .await;

    let plan = create_plan(&stack, ProjectId::new()).await;
    let command = |force, override_reason| Command::SetPlanStatus {
        plan_id: plan,
        status: PlanStatus::Active,
        force,
        override_reason,
    };
    let err = stack
        .handler
        .handle(command(false, None), Actor::user())
        .await
        .expect_err("the rule must block a normal approval");
    assert!(is_blocked(&err, "read-architecture-md message"), "got: {err}");

    let outcome = stack
        .handler
        .handle_with_warnings(
            command(true, Some("urgent production repair".into())),
            Actor::user(),
        )
        .await
        .expect("force + reason must pass an override_allowed plan rule");
    assert!(outcome.events.iter().any(|event| matches!(
        &event.payload,
        Event::PlanStatusChanged {
            plan_id,
            to: PlanStatus::Active,
            ..
        } if *plan_id == plan
    )));
    assert_eq!(outcome.warnings.len(), 1);
    assert_eq!(
        outcome.warnings[0].code,
        "rule_warning:read-architecture-md"
    );
}

#[tokio::test]
async fn blank_plan_override_reason_does_not_bypass_the_rule() {
    let stack = stack().await;
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
            true,
        ),
    )
    .await;

    let plan = create_plan(&stack, ProjectId::new()).await;
    let err = stack
        .handler
        .handle(
            Command::SetPlanStatus {
                plan_id: plan,
                status: PlanStatus::Active,
                force: true,
                override_reason: None,
            },
            Actor::user(),
        )
        .await
        .expect_err("force alone must not override a plan rule");
    assert!(is_blocked(&err, "read-architecture-md"), "got: {err}");

    let err = stack
        .handler
        .handle(
            Command::SetPlanStatus {
                plan_id: plan,
                status: PlanStatus::Active,
                force: true,
                override_reason: Some("   ".into()),
            },
            Actor::user(),
        )
        .await
        .expect_err("a whitespace reason must not override a plan rule");
    assert!(is_blocked(&err, "read-architecture-md"), "got: {err}");
}

#[tokio::test]
async fn non_overridable_plan_rule_poisons_the_whole_override() {
    let stack = stack().await;
    for (rule_key, override_allowed) in [("may-bypass", true), ("no-bypass", false)] {
        install(
            &stack,
            new_rule(
                rule_key,
                RuleScope::Tenant,
                RuleTrigger::PlanBeforeApprove,
                Requirement::ReadArtifact {
                    doc_ref: format!("{rule_key}.md"),
                    min_version: "latest".into(),
                },
                RuleMode::Required,
                override_allowed,
            ),
        )
        .await;
    }

    let plan = create_plan(&stack, ProjectId::new()).await;
    let err = stack
        .handler
        .handle(
            Command::SetPlanStatus {
                plan_id: plan,
                status: PlanStatus::Active,
                force: true,
                override_reason: Some("urgent production repair".into()),
            },
            Actor::user(),
        )
        .await
        .expect_err("a non-overridable plan rule must survive force + reason");
    assert!(is_blocked(&err, "may-bypass"), "got: {err}");
    assert!(is_blocked(&err, "no-bypass"), "got: {err}");
}

// ── Override (spec §1.5) ────────────────────────────────────────────────────────

#[tokio::test]
async fn override_allowed_rule_passes_with_force_in_commands_path() {
    // `force` without a reason is a silent override, and the gate refuses it:
    // an escape hatch that leaves no trace is indistinguishable from the rule
    // not existing (spec §1.5).
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
                force: true,
                override_reason: None, // force alone, no reason
            },
            Actor::user(),
        )
        .await
        .expect_err("silent force must not bypass a required rule");
    assert!(is_blocked(&err, "completion-note"), "got: {err}");
}

/// `force` + a non-empty reason passes a rule that permits override. This is
/// the whole escape hatch: before `override_reason` had a wire field it was
/// unreachable from any client, so the branch existed only on paper.
#[tokio::test]
async fn force_with_a_reason_overrides_a_rule_that_allows_it() {
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
    stack
        .handler
        .handle(
            Command::SetStatus {
                id: task,
                status: Status::Done,
                force: true,
                override_reason: Some("hotfix: production is down".into()),
            },
            Actor::user(),
        )
        .await
        .expect("force + reason must pass an override_allowed rule");
    assert_eq!(
        stack.handler.tasks.get(task).await.unwrap().unwrap().status,
        Status::Done
    );
}

/// A blank reason is no reason: whitespace must not buy a bypass, or the
/// requirement degrades to "type any character".
#[tokio::test]
async fn a_blank_override_reason_does_not_buy_a_bypass() {
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
                force: true,
                override_reason: Some("   ".into()),
            },
            Actor::user(),
        )
        .await
        .expect_err("a whitespace reason must not override");
    assert!(is_blocked(&err, "completion-note"), "got: {err}");
}

/// One rule that forbids override poisons the whole override, even when every
/// other blocked rule would have allowed it — otherwise the strictest rule in
/// the set could be bypassed by pairing it with a lenient one.
#[tokio::test]
async fn a_single_non_overridable_rule_poisons_the_whole_override() {
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
    install(
        &stack,
        new_rule(
            "no-bypass",
            RuleScope::Tenant,
            RuleTrigger::TaskBeforeComplete,
            Requirement::OwnerRequired,
            RuleMode::Required,
            false,
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
                force: true,
                override_reason: Some("hotfix: production is down".into()),
            },
            Actor::user(),
        )
        .await
        .expect_err("a non-overridable rule must survive force + reason");
    assert!(is_blocked(&err, "completion-note"), "got: {err}");
    assert!(is_blocked(&err, "no-bypass"), "got: {err}");
    assert_eq!(
        stack.handler.tasks.get(task).await.unwrap().unwrap().status,
        Status::Inbox,
        "blocked before persist"
    );
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
                    override_reason: None,
                },
                Actor::user(),
            )
            .await
            .expect_err("same inputs, same block");
        assert!(is_blocked(&err, "completion-note"));
    }
}

// ── Evidence satisfaction (spec §1.3; OSS task 019eb65a-3185) ────────────────────

/// required + matching evidence → the requirement is satisfied → allowed.
#[tokio::test]
async fn required_with_evidence_allows_complete() {
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
    // A completion note is a statement about THIS task, so it is recorded on the
    // task — a tenant-wide one is refused, and would not satisfy the rule anyway.
    record_evidence(
        &stack,
        new_evidence(
            EvidenceKind::CompletionNote,
            RuleScope::Task { id: task },
            None,
        ),
    )
    .await;
    let outcome = stack
        .handler
        .handle_with_warnings(
            Command::SetStatus {
                id: task,
                status: Status::Done,
                force: false,
                override_reason: None,
            },
            Actor::user(),
        )
        .await
        .expect("evidence satisfies the requirement → allowed");
    assert!(outcome.warnings.is_empty(), "satisfied → no warning");
    assert_eq!(
        stack.handler.tasks.get(task).await.unwrap().unwrap().status,
        Status::Done,
        "requirement satisfied by evidence — transition proceeds"
    );
}

/// required + NO evidence → still blocked (the v1-equivalent honest behaviour).
#[tokio::test]
async fn required_without_evidence_blocks_complete() {
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
                force: false,
                override_reason: None,
            },
            Actor::user(),
        )
        .await
        .expect_err("no evidence → required still blocks");
    assert!(is_blocked(&err, "completion-note"), "got: {err}");
}

/// Evidence of the wrong kind does not satisfy a requirement.
#[tokio::test]
async fn evidence_of_wrong_kind_does_not_satisfy() {
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
    // Wrong kind: a risk check does not satisfy a completion-note requirement.
    record_evidence(
        &stack,
        new_evidence(
            EvidenceKind::RiskCheckCompleted,
            RuleScope::Task { id: task },
            None,
        ),
    )
    .await;
    let err = stack
        .handler
        .handle(
            Command::SetStatus {
                id: task,
                status: Status::Done,
                force: false,
                override_reason: None,
            },
            Actor::user(),
        )
        .await
        .expect_err("mismatched evidence kind → still blocked");
    assert!(is_blocked(&err, "completion-note"), "got: {err}");
}

/// A targeted requirement (read_artifact for a named doc) is satisfied only by
/// evidence naming the same target.
#[tokio::test]
async fn targeted_read_artifact_satisfied_by_matching_target() {
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
    record_evidence(
        &stack,
        new_evidence(
            EvidenceKind::DocumentReadAck,
            RuleScope::Tenant,
            Some("architecture.md"),
        ),
    )
    .await;

    let plan = create_plan(&stack, project).await;
    stack
        .handler
        .handle(
            Command::SetPlanStatus {
                plan_id: plan,
                status: PlanStatus::Active,
                force: false,
                override_reason: None,
            },
            Actor::user(),
        )
        .await
        .expect("matching read-ack satisfies the requirement → approve allowed");
}

#[tokio::test]
async fn required_fields_reject_incomplete_evidence_and_explain_why() {
    let stack = stack().await;
    install(&stack, impact_check_rule()).await;
    let task = create_task(&stack, "Touch auth").await;
    let mut evidence = new_evidence(
        EvidenceKind::ImpactAssessment,
        RuleScope::Task { id: task },
        Some("auth-module"),
    );
    evidence.payload = serde_json::json!({"summary": "checked"});
    record_evidence(&stack, evidence).await;

    let check = GateCheck {
        trigger: TriggerEvent::TaskBeforeStart,
        project_id: None,
        task_id: Some(task),
        plan_id: None,
        run_id: None,
        document_id: None,
        handoff_id: None,
        status_from: Some(Status::Inbox),
        status_to: Some(Status::InProgress),
        plan_status_from: None,
        plan_status_to: None,
    };
    let GateDecision::Blocked { details, .. } = stack
        .gate
        .check(&Actor::user(), &check, &GateOverride::default())
        .await
        .unwrap()
    else {
        panic!("missing required payload field must block");
    };
    assert!(details["reason"].as_str().unwrap().contains("risk_level"));
    assert!(details["outcomes"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("risk_level"));

    let mut complete = new_evidence(
        EvidenceKind::ImpactAssessment,
        RuleScope::Task { id: task },
        Some("auth-module"),
    );
    complete.payload = serde_json::json!({"risk_level": "low"});
    record_evidence(&stack, complete).await;
    start(&stack, task)
        .await
        .expect("a non-empty required field must satisfy the rule");
}

#[tokio::test]
async fn read_artifact_min_version_is_enforced_numerically() {
    for (version, expected) in [("1", false), ("3", true), ("v3", true)] {
        let stack = stack().await;
        install(
            &stack,
            new_rule(
                "read-architecture-md",
                RuleScope::Tenant,
                RuleTrigger::TaskBeforeStart,
                Requirement::ReadArtifact {
                    doc_ref: "architecture.md".into(),
                    min_version: "3".into(),
                },
                RuleMode::Required,
                false,
            ),
        )
        .await;
        let task = create_task(&stack, "Implement architecture").await;
        let mut evidence = new_evidence(
            EvidenceKind::DocumentReadAck,
            RuleScope::Task { id: task },
            Some("architecture.md"),
        );
        evidence.doc_version = Some(version.into());
        record_evidence(&stack, evidence).await;

        let check = GateCheck {
            trigger: TriggerEvent::TaskBeforeStart,
            project_id: None,
            task_id: Some(task),
            plan_id: None,
            run_id: None,
            document_id: None,
            handoff_id: None,
            status_from: Some(Status::Inbox),
            status_to: Some(Status::InProgress),
            plan_status_from: None,
            plan_status_to: None,
        };
        let decision = stack
            .gate
            .check(&Actor::user(), &check, &GateOverride::default())
            .await
            .unwrap();
        if expected {
            assert!(
                matches!(decision, GateDecision::Allowed),
                "doc_version={version}"
            );
        } else {
            let GateDecision::Blocked { details, .. } = decision else {
                panic!("doc_version={version} must block");
            };
            assert!(details["reason"]
                .as_str()
                .unwrap()
                .contains("older than required 3"));
        }
    }
}

// ── can_start agrees with the gate ──────────────────────────────────────────────
//
// `can_start` exists to answer "may I start this task". Before the gate was
// wired into it, it read only relation blockers, so a task held back by a
// `required` rule came back `ready: true` and the very next `set_status`
// returned `409 rule_blocked`. For an autonomous agent that is the worst kind
// of answer: the one question the tool exists to answer, answered wrongly.

fn impact_check_rule() -> NewRule {
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
    )
}

async fn start(
    stack: &Stack,
    task: TaskId,
) -> Result<Vec<daruma_events::EventEnvelope>, CoreError> {
    stack
        .handler
        .handle(
            Command::SetStatus {
                id: task,
                status: Status::InProgress,
                force: false,
                override_reason: None,
            },
            Actor::user(),
        )
        .await
}

#[tokio::test]
async fn can_start_reports_the_rule_that_blocks_the_transition() {
    let stack = stack().await;
    install(&stack, impact_check_rule()).await;
    let task = create_task(&stack, "Touch auth").await;

    let readiness = stack.can_start(task).await;

    assert!(
        !readiness.ready,
        "unsatisfied required rule must not be ready"
    );
    assert!(
        readiness.blockers.is_empty(),
        "a rule is not a task blocker: {:?}",
        readiness.blockers
    );
    assert_eq!(
        readiness
            .rule_blockers
            .iter()
            .map(|r| r.rule_key.as_str())
            .collect::<Vec<_>>(),
        vec!["auth-impact-check"],
        "the blocking rule must be named, not just counted: {readiness:?}"
    );
    assert_eq!(readiness.reason, "blocked_by_1_rule(s)");
}

/// The invariant the whole change exists for: whatever `can_start` says is
/// ready must actually survive the gate. Checked in both directions on the same
/// task, so a `can_start` that consults a *different* input than the real
/// transition fails here rather than in production.
#[tokio::test]
async fn can_start_ready_implies_the_transition_passes_the_gate() {
    let stack = stack().await;
    install(&stack, impact_check_rule()).await;
    let task = create_task(&stack, "Touch auth").await;

    // Not ready → the transition must indeed be refused.
    assert!(!stack.can_start(task).await.ready);
    let err = start(&stack, task)
        .await
        .expect_err("rule must block start");
    assert!(is_blocked(&err, "auth-impact-check message"), "got: {err}");

    // Satisfy the requirement the same way a real caller would.
    let mut evidence = new_evidence(
        EvidenceKind::ImpactAssessment,
        RuleScope::Task { id: task },
        Some("auth-module"),
    );
    evidence.payload = serde_json::json!({"risk_level": "low"});
    record_evidence(&stack, evidence).await;

    // Ready → the transition must now succeed. If `can_start` built its gate
    // input differently from the real path, one of these two would disagree.
    let readiness = stack.can_start(task).await;
    assert!(readiness.ready, "evidence must unblock: {readiness:?}");
    assert!(readiness.rule_blockers.is_empty());
    assert_eq!(readiness.reason, "ready");

    start(&stack, task)
        .await
        .expect("ready must mean startable");
    assert_eq!(
        stack.handler.tasks.get(task).await.unwrap().unwrap().status,
        Status::InProgress
    );
}

/// A `recommendation` warns but does not block, so it must never move `ready` —
/// otherwise `can_start` acquires the mirror-image defect: refusing a
/// transition that in fact succeeds.
#[tokio::test]
async fn recommendation_rules_warn_without_making_can_start_unready() {
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
            RuleMode::Recommendation,
            true,
        ),
    )
    .await;
    let task = create_task(&stack, "Touch auth").await;

    let readiness = stack.can_start(task).await;
    assert!(
        readiness.ready,
        "a recommendation must not block: {readiness:?}"
    );
    assert!(readiness.rule_blockers.is_empty());
    assert_eq!(
        readiness
            .rule_warnings
            .iter()
            .map(|r| r.rule_key.as_str())
            .collect::<Vec<_>>(),
        vec!["auth-impact-check"],
        "the advisory rule must still be visible: {readiness:?}"
    );

    start(&stack, task)
        .await
        .expect("recommendation must not block");
}

/// The dry run must see the task's REAL current status: a rule conditioned on
/// `status_from` has to match (or not match) identically on both paths. This is
/// the first thing a hand-built `GateCheck` would get wrong.
#[tokio::test]
async fn can_start_honours_a_rule_conditioned_on_the_status_being_left() {
    let stack = stack().await;
    let mut rule = impact_check_rule();
    // The task below sits in `inbox`, so a rule scoped to leaving `todo` must
    // not fire — for `can_start` exactly as for the transition itself.
    rule.condition = Some(Condition {
        status_from: Some(vec![Status::Todo]),
        status_to: None,
    });
    install(&stack, rule).await;
    let task = create_task(&stack, "Touch auth").await;
    assert_eq!(
        stack.handler.tasks.get(task).await.unwrap().unwrap().status,
        Status::Inbox,
        "precondition: the rule's status_from must not match"
    );

    let readiness = stack.can_start(task).await;
    assert!(
        readiness.ready,
        "condition does not match, so nothing blocks: {readiness:?}"
    );
    start(&stack, task)
        .await
        .expect("and the transition agrees");
}

/// A task already `in_progress`: the real `set_status` is a no-op that emits no
/// events and therefore passes no gate, so `can_start` must not invent blockers.
#[tokio::test]
async fn already_in_progress_is_ready_because_the_transition_is_a_no_op() {
    let stack = stack().await;
    let task = create_task(&stack, "Touch auth").await;
    start(&stack, task).await.expect("first start");

    // Install the blocking rule only now, so it would fire on a real start.
    install(&stack, impact_check_rule()).await;

    let readiness = stack.can_start(task).await;
    assert!(
        readiness.ready,
        "no-op transition is not blocked: {readiness:?}"
    );
    assert!(readiness.rule_blockers.is_empty());
    start(&stack, task)
        .await
        .expect("no-op start must still succeed");
}

// ── Evidence reach (a wide record must not be a global off-switch) ─────────────

/// The defect this cap exists for, end to end: one `evidence_submit` at tenant
/// scope used to clear "task needs acceptance criteria" for every task in the
/// tenant, forever — a global off-switch with a single audit record, wearing the
/// costume of proof.
#[tokio::test]
async fn tenant_wide_acceptance_criteria_cannot_unlock_every_task() {
    let stack = stack().await;
    install(
        &stack,
        new_rule(
            "acceptance-criteria",
            RuleScope::Tenant,
            RuleTrigger::TaskBeforeStart,
            Requirement::AcceptanceCriteriaRequired,
            RuleMode::Required,
            true,
        ),
    )
    .await;
    let task = create_task(&stack, "Ship it").await;

    // The shortcut is accepted — evidence is an immutable audit fact and the
    // engine does not police what a caller chooses to assert — but it is inert:
    // a tenant-wide claim reaches nothing, so the rule stays in force.
    //
    // Enforcing on submission instead was tried and reverted: the matcher falls
    // back to the innermost scope when the chain lacks the kind's level, so any
    // width check on submission makes some legitimate rule unsatisfiable (a
    // `run.before_complete` check carries only `[Tenant]`).
    record_evidence(
        &stack,
        new_evidence(
            EvidenceKind::AcceptanceCriteriaDefined,
            RuleScope::Tenant,
            None,
        ),
    )
    .await;

    // The rule is still in force.
    assert!(!stack.can_start(task).await.ready);

    // Recorded where it belongs, it unlocks that task and only that task.
    record_evidence(
        &stack,
        new_evidence(
            EvidenceKind::AcceptanceCriteriaDefined,
            RuleScope::Task { id: task },
            None,
        ),
    )
    .await;
    assert!(stack.can_start(task).await.ready);

    let other = create_task(&stack, "Something else").await;
    assert!(
        !stack.can_start(other).await.ready,
        "unlocking one task must not unlock the next"
    );
}

/// Knowledge-shaped evidence keeps its inheritance: capping reach per kind must
/// not degenerate into "no inheritance at all", which was the deliberate design.
#[tokio::test]
async fn a_document_read_ack_still_reaches_tenant_wide() {
    let stack = stack().await;
    install(
        &stack,
        new_rule(
            "read-architecture-md",
            RuleScope::Tenant,
            RuleTrigger::TaskBeforeStart,
            Requirement::ReadArtifact {
                doc_ref: "architecture.md".into(),
                min_version: "latest".into(),
            },
            RuleMode::Required,
            true,
        ),
    )
    .await;
    record_evidence(
        &stack,
        new_evidence(
            EvidenceKind::DocumentReadAck,
            RuleScope::Tenant,
            Some("architecture.md"),
        ),
    )
    .await;

    // Two different tasks, one acknowledgement: reading the document once
    // legitimately covers both.
    for title in ["First", "Second"] {
        let task = create_task(&stack, title).await;
        assert!(
            stack.can_start(task).await.ready,
            "{title}: a tenant-wide read ack must still inherit downwards"
        );
    }
}

// ── Blocked errors carry a machine-readable unblock hint ─────────────────────
//
// A blocked transition used to return only the human rule message
// (`rule_blocked: Запусти /grill-with-docs`), so an agent could not tell which
// evidence kind, target and scope would lift the block — and a custom rule
// message hides even the requirement type. The error now ends with
// ` | unblock: <compact JSON array>`, one hint per blocking rule.

/// Parse the `unblock` suffix off a `rule_blocked` error. Panics when the
/// suffix is missing or is not valid JSON — that is the regression guard:
/// a bare human phrase again must fail these tests. Split on the LAST
/// occurrence: the rule `message` is free operator text and may itself
/// contain the ` | unblock: ` separator.
fn unblock_hints(err: &CoreError) -> Vec<serde_json::Value> {
    let msg = err.to_string();
    let (_, suffix) = msg.rsplit_once(" | unblock: ").unwrap_or_else(|| {
        panic!("blocked error must carry a machine-readable unblock hint, got: {msg}")
    });
    serde_json::from_str(suffix)
        .unwrap_or_else(|e| panic!("unblock suffix must be valid JSON ({e}): {suffix}"))
}

#[tokio::test]
async fn creation_hint_excludes_the_entity_that_does_not_exist_yet() {
    let stack = stack().await;
    let project = ProjectId::new();
    install(
        &stack,
        new_rule(
            "plan-decision",
            RuleScope::Tenant,
            RuleTrigger::PlanCreated,
            Requirement::DecisionRecord {
                required_fields: vec![],
            },
            RuleMode::Required,
            false,
        ),
    )
    .await;

    let err = stack
        .handler
        .handle(
            Command::CreatePlan {
                plan: NewPlan::new("Blocked plan", project, Actor::user()),
                external_ref: None,
            },
            Actor::user(),
        )
        .await
        .expect_err("required plan.created rule must block creation");
    let hints = unblock_hints(&err);

    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0]["evidence"]["kind"], "decision_record");
    assert_eq!(
        hints[0]["evidence"]["scope"],
        serde_json::to_value(RuleScope::Project { id: project }).unwrap(),
        "the not-yet-persisted plan id must not be suggested as evidence scope"
    );
}

#[tokio::test]
async fn self_only_creation_requirement_reports_that_it_cannot_be_satisfied() {
    for (override_allowed, expected_note) in [
        (
            true,
            "unsatisfiable before creation; requires override",
        ),
        (
            false,
            "unsatisfiable before creation and this rule forbids override; the rule must be relaxed or disabled",
        ),
    ] {
        let stack = stack().await;
        let project = ProjectId::new();
        install(
            &stack,
            new_rule(
                "plan-owner",
                RuleScope::Tenant,
                RuleTrigger::PlanCreated,
                Requirement::OwnerRequired,
                RuleMode::Required,
                override_allowed,
            ),
        )
        .await;

        let err = stack
            .handler
            .handle(
                Command::CreatePlan {
                    plan: NewPlan::new("Blocked plan", project, Actor::user()),
                    external_ref: None,
                },
                Actor::user(),
            )
            .await
            .expect_err("self-only plan.created rule must block creation");
        let hints = unblock_hints(&err);
        let hint = &hints[0];

        assert_eq!(hint["reach"], "self_only");
        assert_eq!(hint["note"], expected_note);
        assert!(
            hint["evidence"].get("scope").is_none(),
            "no existing scope can satisfy self-only evidence before creation: {hint}"
        );
    }
}

#[tokio::test]
async fn document_read_hint_keeps_innermost_scope_and_exposes_tenant_reach() {
    let stack = stack().await;
    install(
        &stack,
        new_rule(
            "read-architecture-md",
            RuleScope::Tenant,
            RuleTrigger::TaskBeforeStart,
            Requirement::ReadArtifact {
                doc_ref: "architecture.md".into(),
                min_version: "latest".into(),
            },
            RuleMode::Required,
            false,
        ),
    )
    .await;
    let task = create_task(&stack, "Implement architecture").await;

    let err = stack
        .handler
        .handle(
            Command::SetStatus {
                id: task,
                status: Status::InProgress,
                force: false,
                override_reason: None,
            },
            Actor::user(),
        )
        .await
        .expect_err("missing document read acknowledgement must block");
    let hints = unblock_hints(&err);
    let hint = &hints[0];

    assert_eq!(hint["reach"], "tenant");
    assert_eq!(
        hint["evidence"]["scope"],
        serde_json::to_value(RuleScope::Task { id: task }).unwrap(),
        "the actionable default stays innermost even when wider reuse is valid"
    );
}

#[tokio::test]
async fn unblock_suffix_is_never_added_to_allowed_or_warning_results() {
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

    let task = create_task(&stack, "Warned task").await;
    let warning = stack
        .handler
        .handle_with_warnings(
            Command::SetStatus {
                id: task,
                status: Status::Done,
                force: false,
                override_reason: None,
            },
            Actor::user(),
        )
        .await
        .expect("recommendation must proceed");
    assert_eq!(warning.warnings.len(), 1);
    assert!(!warning.warnings[0].message.contains(" | unblock: "));

    let satisfied = create_task(&stack, "Allowed task").await;
    record_evidence(
        &stack,
        new_evidence(
            EvidenceKind::CompletionNote,
            RuleScope::Task { id: satisfied },
            None,
        ),
    )
    .await;
    let allowed = stack
        .handler
        .handle_with_warnings(
            Command::SetStatus {
                id: satisfied,
                status: Status::Done,
                force: false,
                override_reason: None,
            },
            Actor::user(),
        )
        .await
        .expect("satisfied requirement must be allowed");
    assert!(allowed.warnings.is_empty());
}

#[tokio::test]
async fn blocked_details_keep_outcomes_beside_unblock_hints() {
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
            false,
        ),
    )
    .await;
    install(
        &stack,
        new_rule(
            "risk-advisory",
            RuleScope::Tenant,
            RuleTrigger::TaskBeforeComplete,
            Requirement::RiskCheck {
                target: "release".into(),
                required_fields: vec![],
            },
            RuleMode::Recommendation,
            false,
        ),
    )
    .await;
    let task = create_task(&stack, "Blocked task").await;
    let check = GateCheck {
        trigger: TriggerEvent::TaskBeforeComplete,
        project_id: None,
        task_id: Some(task),
        plan_id: None,
        run_id: None,
        document_id: None,
        handoff_id: None,
        status_from: Some(Status::Inbox),
        status_to: Some(Status::Done),
        plan_status_from: None,
        plan_status_to: None,
    };

    let GateDecision::Blocked { details, .. } = stack
        .gate
        .check(&Actor::user(), &check, &GateOverride::default())
        .await
        .expect("gate check")
    else {
        panic!("required rule must block");
    };
    let outcomes = details["outcomes"].as_array().expect("outcomes array");
    let unblock = details["unblock"].as_array().expect("unblock array");

    assert_eq!(outcomes.len(), 2, "blocked outcome plus recommendation");
    assert_eq!(unblock.len(), 1, "only blocked outcomes get hints");
    for (outcome, hint) in outcomes.iter().zip(unblock) {
        assert_eq!(outcome["decision"], "blocked");
        assert_eq!(outcome["rule_key"], hint["rule_key"]);
    }
    assert_eq!(outcomes[1]["decision"], "warning");
}

/// The live case: a custom-message `impact_check` rule blocks
/// plan draft→active; the error must name the rule, the requirement type, the
/// satisfying evidence kind and target, and the plan scope — not project or
/// tenant, which `impact_assessment` does not reach.
#[tokio::test]
async fn blocked_error_tells_the_agent_which_evidence_unblocks_it() {
    let stack = stack().await;
    let project = ProjectId::new();
    install(
        &stack,
        new_rule(
            "grill-with-docs",
            RuleScope::Tenant,
            RuleTrigger::PlanBeforeApprove,
            Requirement::ImpactCheck {
                target: "release".into(),
                required_fields: vec![],
            },
            RuleMode::Required,
            false,
        ),
    )
    .await;

    let plan = create_plan(&stack, project).await;
    let approve = || {
        stack.handler.handle(
            Command::SetPlanStatus {
                plan_id: plan,
                status: PlanStatus::Active,
                force: false,
                override_reason: None,
            },
            Actor::user(),
        )
    };
    let err = approve().await.expect_err("unsatisfied rule must block");

    let msg = err.to_string();
    assert!(
        msg.starts_with("conflict: rule_blocked: "),
        "code and prefix must not change: {msg}"
    );
    assert!(
        msg.contains("grill-with-docs message"),
        "the human rule message stays visible: {msg}"
    );

    let hints = unblock_hints(&err);
    assert_eq!(hints.len(), 1, "one blocking rule → one hint: {hints:?}");
    let hint = &hints[0];
    assert_eq!(hint["rule_key"], "grill-with-docs");
    assert_eq!(hint["requirement_type"], "impact_check");
    // The `evidence` object mirrors the `daruma_evidence_submit` argument
    // shape (`{kind, scope, target?}`), so the hint pastes into the call
    // without renaming a single field.
    assert_eq!(hint["evidence"]["kind"], "impact_assessment");
    assert_eq!(hint["evidence"]["target"], "release");
    assert_eq!(
        hint["evidence"]["scope"],
        serde_json::to_value(RuleScope::Plan { id: plan }).unwrap(),
        "impact_assessment reaches the plan, not project/tenant"
    );
    assert_eq!(hint["reach"], "plan", "the hint exposes the kind's ceiling");

    // The hint is actionable, not just well-formed: evidence recorded exactly
    // as told lifts the block.
    record_evidence(
        &stack,
        new_evidence(
            EvidenceKind::ImpactAssessment,
            RuleScope::Plan { id: plan },
            Some("release"),
        ),
    )
    .await;
    approve()
        .await
        .expect("evidence recorded per the hint must unblock the approve");
}

/// Several rules blocking the same transition must ALL appear in the hint
/// list, not just the first. Also pins the self-only/fallback scope: a
/// task-triggered check has no project/plan in its chain, so both hints point
/// at the task itself.
#[tokio::test]
async fn blocked_error_lists_every_blocking_rule_not_just_the_first() {
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
            false,
        ),
    )
    .await;
    install(
        &stack,
        new_rule(
            "risk-check",
            RuleScope::Tenant,
            RuleTrigger::TaskBeforeComplete,
            Requirement::RiskCheck {
                target: "deploy".into(),
                required_fields: vec![],
            },
            RuleMode::Required,
            false,
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
                override_reason: None,
            },
            Actor::user(),
        )
        .await
        .expect_err("both required rules must block");

    let hints = unblock_hints(&err);
    assert_eq!(
        hints.len(),
        2,
        "every blocking rule gets a hint, not just the first: {hints:?}"
    );
    let by_key: std::collections::HashMap<&str, &serde_json::Value> = hints
        .iter()
        .map(|h| (h["rule_key"].as_str().unwrap(), h))
        .collect();
    let note = by_key["completion-note"];
    assert_eq!(note["requirement_type"], "completion_note");
    assert_eq!(note["evidence"]["kind"], "completion_note");
    assert!(
        note["evidence"].get("target").is_none(),
        "a targetless requirement must not invent a target: {note}"
    );
    let risk = by_key["risk-check"];
    assert_eq!(risk["requirement_type"], "risk_check");
    assert_eq!(risk["evidence"]["kind"], "risk_check_completed");
    assert_eq!(risk["evidence"]["target"], "deploy");
    let task_scope = serde_json::to_value(RuleScope::Task { id: task }).unwrap();
    for hint in &hints {
        assert_eq!(
            hint["evidence"]["scope"], task_scope,
            "no plan/project in the chain → scope falls back inwards to the task"
        );
    }
}
