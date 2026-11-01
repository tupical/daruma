//! Lifecycle rules (docs/LIFECYCLE_RULES_SPEC.md). A rule is a *declarative*
//! gate: `event → condition → requirement → allowed | warning | blocked`.
//! There are deliberately no actions, side-effects or chains — the only
//! output of evaluating a rule is a [`crate::rule`] decision the core turns
//! into a `MutationResponse` warning / `rule_blocked` error and an audit
//! event. Anything that gives rules executable behaviour belongs in Cloud,
//! not here (spec §0 anti-goal).
//!
//! This is the OSS v1 surface: only the trigger/condition fields the core can
//! actually evaluate are carried. Reserved fields from the spec (`task_labels`,
//! `affected_modules`, `artifact_kinds`) are intentionally absent until the
//! core grows their carrier — a rule cannot give a false sense of protection.
//! Version checks deliberately accept only an integer with an optional leading
//! `v`/`V`; `latest` means no minimum-version check.

use serde::{Deserialize, Serialize};

use crate::Status;
use daruma_shared::{AgentId, PlanId, ProjectId, RuleId, TaskId, Timestamp};

/// How strictly a rule is enforced (spec §1, `RuleMode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleMode {
    /// Not evaluated at all (spec invariant 2). The default for a disabled
    /// or placeholder rule.
    #[default]
    Off,
    /// Unsatisfied requirement rides `MutationResponse.warnings`; the
    /// mutation still proceeds.
    Recommendation,
    /// Unsatisfied requirement blocks the mutation before persist.
    Required,
}

impl RuleMode {
    /// Strictness ordering for the spec §2 weakening policy:
    /// `Off < Recommendation < Required`. A child-scope rule that *lowers*
    /// strictness is a weakening override and requires the parent rule's
    /// `override_allowed`; raising strictness is always allowed.
    pub fn strictness(self) -> u8 {
        match self {
            RuleMode::Off => 0,
            RuleMode::Recommendation => 1,
            RuleMode::Required => 2,
        }
    }
}

/// Where a rule is *defined* (spec §1, `RuleScope`). Run-level rules do not
/// exist: a run inherits the effective rules of its task (or plan).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuleScope {
    /// Workspace-wide. In self-hosted OSS this is the single `self-hosted`
    /// tenant, i.e. "installation rules".
    Tenant,
    Project {
        id: ProjectId,
    },
    Plan {
        id: PlanId,
    },
    Task {
        id: TaskId,
    },
}

impl RuleScope {
    /// Stable discriminant stored in the `scope_kind` column.
    pub fn kind(&self) -> &'static str {
        match self {
            RuleScope::Tenant => "tenant",
            RuleScope::Project { .. } => "project",
            RuleScope::Plan { .. } => "plan",
            RuleScope::Task { .. } => "task",
        }
    }

    /// Stored id for the scope (`scope_id` column); tenant has no id.
    pub fn id_string(&self) -> Option<String> {
        match self {
            RuleScope::Tenant => None,
            RuleScope::Project { id } => Some(id.to_string()),
            RuleScope::Plan { id } => Some(id.to_string()),
            RuleScope::Task { id } => Some(id.to_string()),
        }
    }
}

/// Lifecycle trigger a rule fires on (spec §1.1), plus `document.created` and
/// `task.handoff` (not yet in the spec table — see
/// `daruma_core::lifecycle_gate::TriggerEvent` doc for their semantics).
/// Mirrors `daruma_core::TriggerEvent`; the wire strings are identical so a
/// rule stored here matches a gate check there without translation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleTrigger {
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
    #[serde(rename = "document.created")]
    DocumentCreated,
    #[serde(rename = "task.handoff")]
    TaskHandoff,
}

/// Targeting predicate (spec §1.2). All fields optional; empty condition = the
/// rule fires on every trigger event in its scope. Semantics: AND across
/// fields, OR within a list.
///
/// Only the v1-evaluable fields are present (status transition, task title
/// prefix). Reserved
/// spec fields (`priority`, `changed_paths`, `task_labels`,
/// `affected_modules`) are omitted by design: their carriers are not on
/// `GateCheck` yet, so storing them would round-trip silently without ever
/// being evaluated. They land with their carrier (see module doc).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Condition {
    /// For `before_*` transitions: the status being left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_from: Option<Vec<Status>>,
    /// For `before_*` transitions: the status being entered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_to: Option<Vec<Status>>,
    /// For `task.*` triggers: the task title starts with one of these
    /// prefixes (e.g. `[Audit]`). Checks without a task never match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_prefix: Option<Vec<String>>,
}

impl Condition {
    pub fn is_empty(&self) -> bool {
        self.status_from.is_none() && self.status_to.is_none() && self.title_prefix.is_none()
    }
}

/// What a rule requires to be demonstrated (spec §1.3). 1:1 with an evidence
/// kind; the evidence registry (OSS task `019eb65a-3185`) records the proof.
/// In v1 (no evidence registry yet) the engine treats every requirement as
/// unsatisfied, so `required` blocks and `recommendation` warns — honest
/// behaviour that strengthens once evidence lands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Requirement {
    /// Read a (versioned) document/artifact. Example rule 1.
    ReadArtifact {
        doc_ref: String,
        /// `latest` or a concrete version number as a string.
        #[serde(default = "default_min_version")]
        min_version: String,
    },
    /// Produce a named artifact kind.
    CreateArtifact { artifact_kind: String },
    /// Assess the impact of a change. Example rule 2.
    ImpactCheck {
        target: String,
        #[serde(default)]
        required_fields: Vec<String>,
    },
    /// Record a decision.
    DecisionRecord {
        #[serde(default)]
        required_fields: Vec<String>,
    },
    /// Attach a who/when/why completion note. Example rule 3.
    CompletionNote {
        #[serde(default)]
        required_fields: Vec<String>,
    },
    /// Task must have an owner.
    OwnerRequired,
    /// Task must declare acceptance criteria.
    AcceptanceCriteriaRequired,
    /// A passing test report for a pinned revision, attested by another authenticated actor.
    IndependentTestVerification {
        executor_id: AgentId,
        source_revision: String,
    },
    /// Task must have ≥1 bound document (`LinkDocumentToTask`) that is not
    /// archived. Satisfied from document state, not from an evidence row: the
    /// binding itself is the proof, so there is nothing to submit.
    DocumentLinked,
    /// Assess risk.
    RiskCheck {
        target: String,
        #[serde(default)]
        required_fields: Vec<String>,
    },
    /// WIP ceiling (ADR-0015): fewer than `max_in_progress` *other* tasks of the
    /// rule's own scope may be `in_progress` when a task starts. Satisfied from
    /// task state, not from an evidence row — a pure read-only predicate over
    /// the core read model, so there is nothing to submit. Only valid on
    /// `task.before_start` and on a scope that holds several tasks (tenant,
    /// project, plan); see [`Requirement::placement_error`].
    WipLimit { max_in_progress: u8 },
}

fn default_min_version() -> String {
    "latest".to_string()
}

impl Requirement {
    /// Every `type` tag, in declaration order — the clause codes of this
    /// policy pack (ADR-0018). A code here without a counterexample in the
    /// rule-engine tests is a red test (`core/tests/it/rule_engine.rs`,
    /// `every_requirement_type_has_a_counterexample`); a variant missing here
    /// is a red test in this crate.
    pub const ALL_TYPES: &'static [&'static str] = &[
        "read_artifact",
        "create_artifact",
        "impact_check",
        "decision_record",
        "completion_note",
        "owner_required",
        "acceptance_criteria_required",
        "independent_test_verification",
        "document_linked",
        "risk_check",
        "wip_limit",
    ];

    /// Stable discriminant (matches the spec `Requirement.type` tag).
    pub fn type_str(&self) -> &'static str {
        match self {
            Requirement::ReadArtifact { .. } => "read_artifact",
            Requirement::CreateArtifact { .. } => "create_artifact",
            Requirement::ImpactCheck { .. } => "impact_check",
            Requirement::DecisionRecord { .. } => "decision_record",
            Requirement::CompletionNote { .. } => "completion_note",
            Requirement::OwnerRequired => "owner_required",
            Requirement::AcceptanceCriteriaRequired => "acceptance_criteria_required",
            Requirement::RiskCheck { .. } => "risk_check",
            Requirement::DocumentLinked => "document_linked",
            Requirement::IndependentTestVerification { .. } => "independent_test_verification",
            Requirement::WipLimit { .. } => "wip_limit",
        }
    }

    /// Why this requirement cannot be placed on `trigger` at `scope`, if it
    /// cannot. Checked at the command boundary (`CreateRule`/`UpdateRule`) so a
    /// rule that could never be evaluated meaningfully is rejected instead of
    /// stored. Only `wip_limit` constrains its placement: it counts tasks
    /// already started, which is a question only a start can ask, and a task
    /// scope holds a single task, so a ceiling there means nothing.
    pub fn placement_error(&self, scope: &RuleScope, trigger: RuleTrigger) -> Option<String> {
        let Requirement::WipLimit { max_in_progress } = self else {
            return None;
        };
        if trigger != RuleTrigger::TaskBeforeStart {
            return Some("wip_limit is only valid on trigger task.before_start".into());
        }
        if matches!(scope, RuleScope::Task { .. }) {
            return Some(
                "wip_limit needs a tenant, project or plan scope; a task scope holds one task"
                    .into(),
            );
        }
        if *max_in_progress == 0 {
            return Some("wip_limit max_in_progress must be at least 1".into());
        }
        None
    }
}

/// A stored lifecycle rule (spec §1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: RuleId,
    /// Stable key for inheritance / override across scope levels (spec §2),
    /// e.g. `completion-note`. Unique within a `(scope_kind, scope_id)`.
    pub rule_key: String,
    pub title: String,
    pub scope: RuleScope,
    pub trigger: RuleTrigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<Condition>,
    pub requirement: Requirement,
    pub mode: RuleMode,
    /// Shown to the executor on warn/block ("what to do to pass").
    #[serde(default)]
    pub message: String,
    /// Whether the rule may be weakened down the hierarchy / bypassed with
    /// `force` + `override_reason` (spec §1.5 / §2).
    #[serde(default)]
    pub override_allowed: bool,
    pub enabled: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Input for `CreateRule`. `id` is server-assigned when absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<RuleId>,
    pub rule_key: String,
    pub title: String,
    pub scope: RuleScope,
    pub trigger: RuleTrigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<Condition>,
    pub requirement: Requirement,
    #[serde(default)]
    pub mode: RuleMode,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub override_allowed: bool,
    /// Defaults to enabled; an `off` mode already means "not evaluated".
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

impl NewRule {
    /// Materialise a stored [`Rule`], assigning an id and timestamps.
    pub fn into_rule(self, now: Timestamp) -> Rule {
        Rule {
            id: self.id.unwrap_or_default(),
            rule_key: self.rule_key,
            title: self.title,
            scope: self.scope,
            trigger: self.trigger,
            condition: self.condition,
            requirement: self.requirement,
            mode: self.mode,
            message: self.message,
            override_allowed: self.override_allowed,
            enabled: self.enabled,
            created_at: now,
            updated_at: now,
        }
    }
}

/// Partial update for an existing rule (`UpdateRule`). `None` leaves a field
/// unchanged. `scope`, `trigger` and `rule_key` are immutable identity — to
/// change them, create a new rule.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<Option<Condition>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirement: Option<Requirement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<RuleMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_allowed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl RulePatch {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.condition.is_none()
            && self.requirement.is_none()
            && self.mode.is_none()
            && self.message.is_none()
            && self.override_allowed.is_none()
            && self.enabled.is_none()
    }

    /// Apply the patch to a rule, refreshing `updated_at`.
    pub fn apply(self, mut rule: Rule, now: Timestamp) -> Rule {
        if let Some(v) = self.title {
            rule.title = v;
        }
        if let Some(v) = self.condition {
            rule.condition = v;
        }
        if let Some(v) = self.requirement {
            rule.requirement = v;
        }
        if let Some(v) = self.mode {
            rule.mode = v;
        }
        if let Some(v) = self.message {
            rule.message = v;
        }
        if let Some(v) = self.override_allowed {
            rule.override_allowed = v;
        }
        if let Some(v) = self.enabled {
            rule.enabled = v;
        }
        rule.updated_at = now;
        rule
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_default_is_off() {
        assert_eq!(RuleMode::default(), RuleMode::Off);
    }

    #[test]
    fn requirement_round_trips_with_type_tag() {
        let r = Requirement::CompletionNote {
            required_fields: vec!["actor".into(), "reason".into()],
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"type\":\"completion_note\""), "got {json}");
        let back: Requirement = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
        assert_eq!(r.type_str(), "completion_note");
    }

    /// ADR-0018: `ALL_TYPES` is the variant list serde derives, not a copy
    /// that may drift — serde names every variant when it rejects a tag.
    #[test]
    fn all_types_lists_every_variant_in_declaration_order() {
        let err = serde_json::from_str::<Requirement>(r#"{"type":"__probe__"}"#)
            .expect_err("unknown tag")
            .to_string();
        let (_, expected) = err
            .split_once("expected one of ")
            .unwrap_or_else(|| panic!("serde lists the variants, got: {err}"));
        let variants: Vec<&str> = expected
            .split(", ")
            .map(|v| v.split('`').nth(1).expect("backticked variant"))
            .collect();
        assert_eq!(variants, Requirement::ALL_TYPES);
    }

    #[test]
    fn document_linked_is_a_unit_type_tag() {
        let r: Requirement = serde_json::from_str(r#"{"type":"document_linked"}"#).unwrap();
        assert_eq!(r, Requirement::DocumentLinked);
        assert_eq!(r.type_str(), "document_linked");
    }

    #[test]
    fn wip_limit_wire_form() {
        let r: Requirement =
            serde_json::from_str(r#"{"type":"wip_limit","max_in_progress":2}"#).unwrap();
        assert_eq!(r, Requirement::WipLimit { max_in_progress: 2 });
        assert_eq!(r.type_str(), "wip_limit");
    }

    #[test]
    fn wip_limit_placement_is_start_only_and_multi_task_scope() {
        let wip = Requirement::WipLimit { max_in_progress: 1 };
        let project = RuleScope::Project {
            id: ProjectId::new(),
        };
        assert_eq!(
            wip.placement_error(&project, RuleTrigger::TaskBeforeStart),
            None
        );
        assert_eq!(
            wip.placement_error(&RuleScope::Tenant, RuleTrigger::TaskBeforeStart),
            None
        );
        assert!(wip
            .placement_error(&project, RuleTrigger::TaskBeforeComplete)
            .is_some());
        assert!(wip
            .placement_error(
                &RuleScope::Task { id: TaskId::new() },
                RuleTrigger::TaskBeforeStart
            )
            .is_some());
        assert!(Requirement::WipLimit { max_in_progress: 0 }
            .placement_error(&project, RuleTrigger::TaskBeforeStart)
            .is_some());
        // Every other requirement keeps its free placement.
        assert_eq!(
            Requirement::DocumentLinked.placement_error(
                &RuleScope::Task { id: TaskId::new() },
                RuleTrigger::TaskBeforeComplete
            ),
            None
        );
    }

    #[test]
    fn read_artifact_min_version_defaults_to_latest() {
        let r: Requirement =
            serde_json::from_str(r#"{"type":"read_artifact","doc_ref":"architecture.md"}"#)
                .unwrap();
        match r {
            Requirement::ReadArtifact { min_version, .. } => assert_eq!(min_version, "latest"),
            other => panic!("expected read_artifact, got {other:?}"),
        }
    }

    #[test]
    fn scope_round_trips() {
        let s = RuleScope::Project {
            id: ProjectId::new(),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"kind\":\"project\""), "got {json}");
        let back: RuleScope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        assert_eq!(s.kind(), "project");
        assert!(s.id_string().is_some());
        assert!(RuleScope::Tenant.id_string().is_none());
    }

    #[test]
    fn patch_merges_and_leaves_unset_fields() {
        let now = daruma_shared::time::now();
        let rule = NewRule {
            id: None,
            rule_key: "completion-note".into(),
            title: "t".into(),
            scope: RuleScope::Tenant,
            trigger: RuleTrigger::TaskBeforeComplete,
            condition: None,
            requirement: Requirement::CompletionNote {
                required_fields: vec![],
            },
            mode: RuleMode::Required,
            message: "m".into(),
            override_allowed: true,
            enabled: true,
        }
        .into_rule(now);

        let patched = RulePatch {
            mode: Some(RuleMode::Recommendation),
            ..Default::default()
        }
        .apply(rule.clone(), now);
        assert_eq!(patched.mode, RuleMode::Recommendation);
        assert_eq!(patched.title, "t", "unset field unchanged");
        assert!(patched.override_allowed);
    }
}
