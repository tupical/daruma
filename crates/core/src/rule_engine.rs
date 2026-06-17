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
//! the check and the stored rules. Evidence-based *satisfaction* (spec §1.3)
//! lands with the evidence registry (OSS task `019eb65a-3185`); until then a
//! requirement is treated as unsatisfied, so `required` blocks and
//! `recommendation` warns — honest, and it only gets stricter once evidence
//! can be recorded. Override (`force` + `override_reason`) is honoured per
//! spec §1.5 for rules with `override_allowed=true`.
//!
//! Zero-cost when no rules exist: a check whose scope chain has no matching
//! rows resolves to `Allowed` after one indexed query (the handler only calls
//! the gate at all when one is wired).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use taskagent_api_dto::MutationWarning;
use taskagent_domain::{Actor, Condition, Rule, RuleMode, RuleScope, RuleTrigger};
use taskagent_shared::Result;

use crate::lifecycle_gate::{GateCheck, GateDecision, GateOverride, LifecycleGate, TriggerEvent};
use crate::repos::RuleRepository;

/// Rule engine over a [`RuleRepository`].
pub struct RuleEngineGate {
    rules: Arc<dyn RuleRepository>,
}

impl RuleEngineGate {
    pub fn new(rules: Arc<dyn RuleRepository>) -> Self {
        Self { rules }
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
/// list. Fields whose carrier the check does not provide (e.g. `changed_paths`
/// before the evidence registry) are treated as *not constraining* in v1 —
/// the spec marks them as activating with their carrier; a condition that
/// relies solely on such a field still fires (the requirement then governs).
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
    // `priority` and `changed_paths` carriers are not on GateCheck in v1;
    // they round-trip in storage and activate with their carrier (spec §1.2).
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
