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

use daruma_core::lifecycle_gate::LifecycleGate;
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

    // Evidence of the matching kind at the (tenant) scope the rule fires in.
    record_evidence(
        &stack,
        new_evidence(EvidenceKind::CompletionNote, RuleScope::Tenant, None),
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
    // Wrong kind: a risk check does not satisfy a completion-note requirement.
    record_evidence(
        &stack,
        new_evidence(EvidenceKind::RiskCheckCompleted, RuleScope::Tenant, None),
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
            },
            Actor::user(),
        )
        .await
        .expect("matching read-ack satisfies the requirement → approve allowed");
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
    record_evidence(
        &stack,
        new_evidence(
            EvidenceKind::ImpactAssessment,
            RuleScope::Tenant,
            Some("auth-module"),
        ),
    )
    .await;

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
