//! Rule-engine gate — the deterministic [`LifecycleGate`] implementation
//! backed by stored lifecycle rules (docs/LIFECYCLE_RULES_SPEC.md).
//!
//! Flow for one [`GateCheck`]: map the trigger, assemble the scope chain from
//! the entity refs the check carries, ask the repo for effective enabled rules
//! (`mode != off`), match each rule's condition against the check, then turn
//! matched rules into a decision:
//!
//! - `required`    → [`GateDecision::Blocked`]
//! - `recommendation` → [`GateDecision::Warning`]
//!
//! Determinism (spec invariant 8): no clock, no network — the only inputs are
//! the check, the stored rules, and recorded evidence. Evidence-based
//! *satisfaction* (spec §1.3) is wired through an [`EvidenceRepository`]: for
//! each `required` rule whose condition matches, the gate maps the rule's
//! [`Requirement`] to the evidence kind that satisfies it and asks the registry
//! whether live (non-superseded) evidence exists anywhere in the scope chain. A
//! satisfied requirement drops the rule (the transition is allowed); an
//! unsatisfied one blocks. With no evidence repo wired the gate degrades to the
//! honest v1 behaviour (every requirement unsatisfied → `required` blocks).
//! Override (`force` + `override_reason`) is honoured per spec §1.5 for rules
//! with `override_allowed=true`.
//!
//! Zero-cost when no rules exist: a check whose scope chain has no matching
//! rows resolves to `Allowed` after one indexed query (the handler only calls
//! the gate at all when one is wired). Evidence is only queried for rules that
//! both match and would otherwise block, so an unconstrained workspace pays
//! nothing extra.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use taskagent_api_dto::MutationWarning;
use taskagent_domain::{
    Actor, Condition, EvidenceKind, Requirement, Rule, RuleMode, RuleScope, RuleTrigger,
};
use taskagent_shared::Result;

use crate::lifecycle_gate::{GateCheck, GateDecision, GateOverride, LifecycleGate, TriggerEvent};
use crate::repos::{EvidenceRepository, RuleRepository};

/// Rule engine over a [`RuleRepository`] and, optionally, an
/// [`EvidenceRepository`] for requirement satisfaction.
pub struct RuleEngineGate {
    rules: Arc<dyn RuleRepository>,
    evidence: Option<Arc<dyn EvidenceRepository>>,
}

impl RuleEngineGate {
    /// Construct without evidence: every requirement is treated as unsatisfied
    /// (honest v1 behaviour). Prefer [`RuleEngineGate::with_evidence`].
    pub fn new(rules: Arc<dyn RuleRepository>) -> Self {
        Self {
            rules,
            evidence: None,
        }
    }

    /// Construct with an evidence registry so satisfied `required` requirements
    /// unblock the transition (spec §1.3).
    pub fn with_evidence(
        rules: Arc<dyn RuleRepository>,
        evidence: Arc<dyn EvidenceRepository>,
    ) -> Self {
        Self {
            rules,
            evidence: Some(evidence),
        }
    }

    /// Is `rule`'s requirement satisfied by recorded evidence in `chain`? With
    /// no evidence repo wired, nothing is ever satisfied (returns `false`).
    async fn requirement_satisfied(&self, rule: &Rule, chain: &[RuleScope]) -> Result<bool> {
        let Some(evidence) = &self.evidence else {
            return Ok(false);
        };
        let (kind, target) = requirement_evidence(&rule.requirement);
        evidence
            .has_live_evidence(chain, kind, target.as_deref())
            .await
    }

    /// Build the scope chain (outermost → innermost) for a check. Tenant is
    /// always the root; project/plan/task are appended when the check carries
    /// their id. Run-level checks inherit via the plan/task refs the gate
    /// derivation already attached (spec §1: runs have no own scope).
    fn scope_chain(check: &GateCheck) -> Vec<RuleScope> {
        let mut chain = vec![RuleScope::Tenant];
        if let Some(project_id) = check.project_id {
            chain.push(RuleScope::Project { id: project_id });
        }
        if let Some(plan_id) = check.plan_id {
            chain.push(RuleScope::Plan { id: plan_id });
        }
        if let Some(task_id) = check.task_id {
            chain.push(RuleScope::Task { id: task_id });
        }
        chain
    }
}

#[async_trait]
impl LifecycleGate for RuleEngineGate {
    async fn check(
        &self,
        _actor: &Actor,
        check: &GateCheck,
        gate_override: &GateOverride,
    ) -> Result<GateDecision> {
        let trigger = map_trigger(check.trigger);
        let chain = Self::scope_chain(check);
        let candidates = self.rules.effective_rules(&chain, trigger).await?;

        let mut warnings = Vec::new();
        let mut blocked: Vec<&Rule> = Vec::new();
        // Any blocked rule that forbids override poisons the whole override.
        let mut override_forbidden = false;

        for rule in &candidates {
            if !condition_matches(rule.condition.as_ref(), check) {
                continue;
            }
            // Spec §1.3: a requirement backed by recorded evidence is satisfied,
            // so the rule neither blocks nor warns. Only consult the registry
            // for rules that would otherwise act (`off` is inert).
            if rule.mode != RuleMode::Off && self.requirement_satisfied(rule, &chain).await? {
                continue;
            }
            match rule.mode {
                RuleMode::Off => {}
                RuleMode::Recommendation => warnings.push(rule_warning(rule)),
                RuleMode::Required => {
                    if !rule.override_allowed {
                        override_forbidden = true;
                    }
                    blocked.push(rule);
                }
            }
        }

        if blocked.is_empty() {
            return Ok(if warnings.is_empty() {
                GateDecision::Allowed
            } else {
                GateDecision::Warning(warnings)
            });
        }

        // Override path (spec §1.5): force + non-empty reason passes blocked
        // rules, but only when *every* blocked rule allows override.
        let override_ok = gate_override.force
            && gate_override
                .override_reason
                .as_deref()
                .map(|r| !r.trim().is_empty())
                .unwrap_or(false)
            && !override_forbidden;
        if override_ok {
            // Overridden blocks degrade to warnings so the executor still sees
            // what was bypassed; the RuleOverridden audit trail lands with the
            // evidence registry task.
            for rule in blocked {
                warnings.push(rule_warning(rule));
            }
            return Ok(if warnings.is_empty() {
                GateDecision::Allowed
            } else {
                GateDecision::Warning(warnings)
            });
        }

        // Build the structured outcome list (spec §1.5): all blocked first,
        // then warnings, so a client can show every requirement at once.
        let outcomes: Vec<serde_json::Value> = blocked
            .iter()
            .map(|r| rule_outcome(r, "blocked"))
            .chain(warnings.iter().map(|w| {
                json!({
                    "rule_key": w.code.strip_prefix("rule_warning:").unwrap_or(&w.code),
                    "decision": "warning",
                    "message": w.message,
                })
            }))
            .collect();
        let first = blocked[0];
        Ok(GateDecision::Blocked {
            message: blocked_message(&blocked),
            details: json!({
                "rule_id": first.id.to_string(),
                "rule_key": first.rule_key,
                "requirement": first.requirement,
                "outcomes": outcomes,
            }),
        })
    }
}

/// Map a [`Requirement`] to the evidence kind (and optional target) that
/// satisfies it. The kind strings match `EvidenceKind::as_str()`, so a rule and
/// the evidence proving it line up without translation (spec §1.3). The target
/// narrows the match (e.g. the document a `read_artifact` rule names); `None`
/// means any evidence of that kind in scope satisfies the rule.
fn requirement_evidence(req: &Requirement) -> (EvidenceKind, Option<String>) {
    match req {
        Requirement::ReadArtifact { doc_ref, .. } => {
            (EvidenceKind::DocumentReadAck, Some(doc_ref.clone()))
        }
        Requirement::CreateArtifact { artifact_kind } => {
            (EvidenceKind::ArtifactCreated, Some(artifact_kind.clone()))
        }
        Requirement::ImpactCheck { target, .. } => {
            (EvidenceKind::ImpactAssessment, Some(target.clone()))
        }
        Requirement::DecisionRecord { .. } => (EvidenceKind::DecisionRecord, None),
        Requirement::CompletionNote { .. } => (EvidenceKind::CompletionNote, None),
        Requirement::OwnerRequired => (EvidenceKind::OwnerAssigned, None),
        Requirement::AcceptanceCriteriaRequired => (EvidenceKind::AcceptanceCriteriaDefined, None),
        Requirement::RiskCheck { target, .. } => {
            (EvidenceKind::RiskCheckCompleted, Some(target.clone()))
        }
    }
}

fn map_trigger(t: TriggerEvent) -> RuleTrigger {
    match t {
        TriggerEvent::ProjectCreated => RuleTrigger::ProjectCreated,
        TriggerEvent::PlanCreated => RuleTrigger::PlanCreated,
        TriggerEvent::PlanBeforeApprove => RuleTrigger::PlanBeforeApprove,
        TriggerEvent::TaskCreated => RuleTrigger::TaskCreated,
        TriggerEvent::TaskBeforeStart => RuleTrigger::TaskBeforeStart,
        TriggerEvent::TaskBeforeComplete => RuleTrigger::TaskBeforeComplete,
        TriggerEvent::RunBeforeExecute => RuleTrigger::RunBeforeExecute,
        TriggerEvent::RunBeforeComplete => RuleTrigger::RunBeforeComplete,
    }
}

/// Match a rule condition against a check (spec §1.2 v1 fields). Empty / `None`
/// condition matches everything. Semantics: AND across fields, OR within a
/// list. Only the status-transition fields exist in v1; the spec's other
/// targeting fields (priority, changed_paths, …) are omitted from
/// [`Condition`] until their carrier reaches `GateCheck`.
fn condition_matches(condition: Option<&Condition>, check: &GateCheck) -> bool {
    let Some(cond) = condition else {
        return true;
    };
    if cond.is_empty() {
        return true;
    }
    if let Some(allowed) = &cond.status_from {
        match check.status_from {
            Some(s) => {
                if !allowed.contains(&s) {
                    return false;
                }
            }
            None => return false,
        }
    }
    if let Some(allowed) = &cond.status_to {
        match check.status_to {
            Some(s) => {
                if !allowed.contains(&s) {
                    return false;
                }
            }
            None => return false,
        }
    }
    true
}

fn rule_warning(rule: &Rule) -> MutationWarning {
    MutationWarning {
        code: format!("rule_warning:{}", rule.rule_key),
        message: warn_message(rule),
        details: json!({
            "rule_id": rule.id.to_string(),
            "rule_key": rule.rule_key,
            "requirement": rule.requirement,
        }),
    }
}

fn rule_outcome(rule: &Rule, decision: &str) -> serde_json::Value {
    json!({
        "rule_id": rule.id.to_string(),
        "rule_key": rule.rule_key,
        "decision": decision,
        "message": warn_message(rule),
        "requirement": rule.requirement,
    })
}

fn warn_message(rule: &Rule) -> String {
    if rule.message.trim().is_empty() {
        format!(
            "rule `{}` requires `{}`",
            rule.rule_key,
            rule.requirement.type_str()
        )
    } else {
        rule.message.clone()
    }
}

fn blocked_message(blocked: &[&Rule]) -> String {
    if blocked.len() == 1 {
        warn_message(blocked[0])
    } else {
        let keys: Vec<&str> = blocked.iter().map(|r| r.rule_key.as_str()).collect();
        format!(
            "{} rules block this transition: {}",
            blocked.len(),
            keys.join(", ")
        )
    }
}
