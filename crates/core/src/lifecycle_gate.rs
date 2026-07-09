//! Lifecycle gate — pluggable pre-persist checks for lifecycle transitions.
//!
//! Contract (docs/LIFECYCLE_RULES_SPEC.md §1.1/§1.5): on selected lifecycle
//! points a gate can return `allowed | warning | blocked`. Warnings ride the
//! existing [`MutationResponse::warnings`] channel; `blocked` aborts the
//! command BEFORE persist with `CoreError::Conflict("rule_blocked: …")`.
//!
//! Trigger points are derived from the events a command *would* persist
//! (built but not yet appended), so every path to a transition is covered by
//! a single call site in [`CommandHandler::handle_with_warnings`] — e.g.
//! `SetStatus(done)`, `CompleteTask` and `plan_drain_next` all emit
//! `TaskStatusChanged` and therefore all hit `task.before_complete` /
//! `task.before_start` without per-arm wiring.
//!
//! No gate wired (the default) is zero-cost: the handler skips derivation
//! entirely. The rule engine implements [`LifecycleGate`]; tests use stubs.
//!
//! [`MutationResponse::warnings`]: daruma_api_dto::MutationResponse

use async_trait::async_trait;
use daruma_api_dto::MutationWarning;
use daruma_domain::{Actor, PlanStatus, Status};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{PlanId, ProjectId, Result, RunId, TaskId};
use serde::{Deserialize, Serialize};

use crate::Command;

/// Lifecycle trigger taxonomy, v1-active subset (spec §1.1). Reserved
/// events (`plan.before_start`, `run.created`, `decision.created`,
/// `artifact.*`) are intentionally absent until the core grows their
/// command paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TriggerEvent {
    #[serde(rename = "project.created")]
    ProjectCreated,
    #[serde(rename = "plan.created")]
    PlanCreated,
    #[serde(rename = "plan.before_approve")]
    PlanBeforeApprove,
    #[serde(rename = "task.created")]
    TaskCreated,
    #[serde(rename = "task.before_start")]
    TaskBeforeStart,
    #[serde(rename = "task.before_complete")]
    TaskBeforeComplete,
    #[serde(rename = "run.before_execute")]
    RunBeforeExecute,
    #[serde(rename = "run.before_complete")]
    RunBeforeComplete,
}

impl TriggerEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            TriggerEvent::ProjectCreated => "project.created",
            TriggerEvent::PlanCreated => "plan.created",
            TriggerEvent::PlanBeforeApprove => "plan.before_approve",
            TriggerEvent::TaskCreated => "task.created",
            TriggerEvent::TaskBeforeStart => "task.before_start",
            TriggerEvent::TaskBeforeComplete => "task.before_complete",
            TriggerEvent::RunBeforeExecute => "run.before_execute",
            TriggerEvent::RunBeforeComplete => "run.before_complete",
        }
    }
}

/// One derived check point: the trigger plus entity refs and (for status
/// transitions) the from/to pair, taken from the *not yet persisted* events
/// built for the current command.
#[derive(Debug, Clone, Serialize)]
pub struct GateCheck {
    pub trigger: TriggerEvent,
    pub project_id: Option<ProjectId>,
    pub task_id: Option<TaskId>,
    pub plan_id: Option<PlanId>,
    pub run_id: Option<RunId>,
    pub status_from: Option<Status>,
    pub status_to: Option<Status>,
    pub plan_status_from: Option<PlanStatus>,
    pub plan_status_to: Option<PlanStatus>,
}

impl GateCheck {
    fn new(trigger: TriggerEvent) -> Self {
        Self {
            trigger,
            project_id: None,
            task_id: None,
            plan_id: None,
            run_id: None,
            status_from: None,
            status_to: None,
            plan_status_from: None,
            plan_status_to: None,
        }
    }
}

/// Command-level override signal (spec §1.5). `force` alone only soft-acks
/// `can_start` blockers; passing a *blocked rule* additionally requires a
/// non-empty `override_reason` and per-rule `override_allowed` — both
/// enforced by the gate implementation, which also records `RuleOverridden`.
///
/// `override_reason` has no wire field yet — it lands together with the rule
/// engine (`RuleOverridden` event); the contract is fixed here so the trait
/// does not change.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GateOverride {
    pub force: bool,
    pub override_reason: Option<String>,
}

/// Extract the override signal from a command (spec §1.5).
pub fn gate_override_of(cmd: &Command) -> GateOverride {
    match cmd {
        Command::SetStatus { force, .. } => GateOverride {
            force: *force,
            override_reason: None,
        },
        _ => GateOverride::default(),
    }
}

/// Decision returned by a gate for a single [`GateCheck`].
#[derive(Debug, Clone)]
pub enum GateDecision {
    Allowed,
    /// Mutation proceeds; warnings ride `MutationResponse.warnings`.
    Warning(Vec<MutationWarning>),
    /// Mutation is rejected before persist. `message` is shown to the
    /// executor ("what to do to pass"); `details` may carry the structured
    /// outcome list (all blocked rules) for richer clients.
    Blocked {
        message: String,
        details: serde_json::Value,
    },
}

/// Pre-persist lifecycle gate. Implementations must be read-only and
/// deterministic (spec §0 anti-goal checklist, §3 invariant 8): no nested
/// commands, no mutations — the only outputs are the decision and the
/// audit events the *core* (not the gate) emits.
#[async_trait]
pub trait LifecycleGate: Send + Sync {
    async fn check(
        &self,
        actor: &Actor,
        check: &GateCheck,
        gate_override: &GateOverride,
    ) -> Result<GateDecision>;
}

/// Outcome of [`crate::CommandHandler::handle_with_warnings`].
#[derive(Debug)]
pub struct DispatchOutcome {
    pub events: Vec<EventEnvelope>,
    pub warnings: Vec<MutationWarning>,
}

/// Derive gate checks from built (not yet persisted) events. Pure function:
/// the mapping is the single source of truth for trigger coverage — any
/// command emitting these events is gated, including bulk and drain paths.
pub fn derive_gate_checks(events: &[Event]) -> Vec<GateCheck> {
    let mut checks = Vec::new();
    for event in events {
        match event {
            Event::ProjectCreated { project } => {
                let mut check = GateCheck::new(TriggerEvent::ProjectCreated);
                check.project_id = Some(project.id);
                checks.push(check);
            }
            Event::PlanCreated { plan } => {
                let mut check = GateCheck::new(TriggerEvent::PlanCreated);
                check.plan_id = Some(plan.id);
                check.project_id = Some(plan.project_id);
                checks.push(check);
            }
            Event::PlanStatusChanged {
                plan_id, from, to, ..
            } if *from == PlanStatus::Draft && *to == PlanStatus::Active => {
                let mut check = GateCheck::new(TriggerEvent::PlanBeforeApprove);
                check.plan_id = Some(*plan_id);
                check.plan_status_from = Some(*from);
                check.plan_status_to = Some(*to);
                checks.push(check);
            }
            Event::TaskCreated { task } => {
                let mut check = GateCheck::new(TriggerEvent::TaskCreated);
                check.task_id = task.id;
                check.project_id = task.project_id;
                checks.push(check);
            }
            Event::TaskStatusChanged { task_id, from, to }
                if *to == Status::InProgress || *to == Status::Done =>
            {
                let trigger = if *to == Status::InProgress {
                    TriggerEvent::TaskBeforeStart
                } else {
                    TriggerEvent::TaskBeforeComplete
                };
                let mut check = GateCheck::new(trigger);
                check.task_id = Some(*task_id);
                check.status_from = Some(*from);
                check.status_to = Some(*to);
                checks.push(check);
            }
            Event::RunStarted { run } => {
                let mut check = GateCheck::new(TriggerEvent::RunBeforeExecute);
                check.run_id = Some(run.id);
                check.plan_id = Some(run.plan_id);
                checks.push(check);
            }
            Event::RunCompleted { run_id, .. } => {
                let mut check = GateCheck::new(TriggerEvent::RunBeforeComplete);
                check.run_id = Some(*run_id);
                checks.push(check);
            }
            _ => {}
        }
    }
    checks
}
