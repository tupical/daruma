//! Plan source umbrella at intake (ADR-0009): resolve `source_ref`, check it
//! against the project's `intake_source` policy, auto-parent repeat sources.

use daruma_api_dto::MutationWarning;
use daruma_domain::{
    normalize_source_ref, GitContext, IntakeSourceMode, IntakeSourcePolicy, NewPlan, SourceChannel,
};
use daruma_shared::{CoreError, PlanId, Result};
use daruma_storage::PlanRepo;
use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};

use crate::{plan_concurrency::detect_parent_cycle, CommandHandler};

const MAX_PATTERN_LEN: usize = 512;
const MAX_DERIVE_RULES: usize = 32;
const MAX_CHANNELS: usize = 32;

fn compile(pattern: &str, what: &str) -> Result<Regex> {
    if pattern.chars().count() > MAX_PATTERN_LEN {
        return Err(invalid(format!(
            "{what} is longer than {MAX_PATTERN_LEN} characters"
        )));
    }
    RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .build()
        .map_err(|e| invalid(format!("{what} `{pattern}` is not a valid regex: {e}")))
}

fn invalid(message: String) -> CoreError {
    CoreError::unprocessable("invalid_intake_source", message, Value::Null)
}

/// Reject a policy that could never be applied: bad or oversized regexes,
/// too many rules, unknown `derive.from`, empty channel schemes, refs that
/// are not URIs.
pub(crate) fn validate_policy(policy: &IntakeSourcePolicy) -> Result<()> {
    if policy.derive.len() > MAX_DERIVE_RULES {
        return Err(invalid(format!(
            "at most {MAX_DERIVE_RULES} derive rules are allowed"
        )));
    }
    if policy.channels.len() > MAX_CHANNELS {
        return Err(invalid(format!(
            "at most {MAX_CHANNELS} channels are allowed"
        )));
    }
    if let Some(default_ref) = &policy.default_ref {
        normalize_source_ref(default_ref).map_err(|e| invalid(format!("default_ref: {e}")))?;
    }
    for rule in &policy.derive {
        if rule.from != "branch" {
            return Err(invalid(format!(
                "derive.from `{}` is not supported (only `branch`)",
                rule.from
            )));
        }
        compile(&rule.pattern, "derive.pattern")?;
        normalize_source_ref(&rule.ref_template)
            .map_err(|e| invalid(format!("derive.ref: {e}")))?;
    }
    for channel in &policy.channels {
        if channel.scheme.trim().is_empty() {
            return Err(invalid("channels[].scheme must not be empty".into()));
        }
        if let Some(pattern) = &channel.pattern {
            compile(pattern, "channels[].pattern")?;
        }
    }
    Ok(())
}

/// Substitute `$1..$9` with the capture groups. `None` when the template
/// names a group that did not participate in the match: the rule does not
/// fire rather than producing a ref with a hole in it.
fn expand(template: &str, caps: &regex::Captures<'_>) -> Option<String> {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match chars.peek().and_then(|d| d.to_digit(10)).filter(|&d| d > 0) {
            Some(group) if c == '$' => {
                chars.next();
                out.push_str(caps.get(group as usize)?.as_str());
            }
            _ => out.push(c),
        }
    }
    Some(out)
}

fn channel_matches(channel: &SourceChannel, source_ref: &str) -> Result<bool> {
    let scheme = source_ref.split(':').next().unwrap_or_default();
    if !scheme.eq_ignore_ascii_case(channel.scheme.trim()) {
        return Ok(false);
    }
    match &channel.pattern {
        Some(pattern) => Ok(compile(pattern, "channels[].pattern")?.is_match(source_ref)),
        None => Ok(true),
    }
}

fn normalized(source_ref: &str) -> Result<String> {
    normalize_source_ref(source_ref).map_err(CoreError::validation)
}

/// Shape checks every plan-creating command gets, with or without policy:
/// the legacy-intake marker is reserved for storage, an explicit ref is
/// normalised, and `git_context` is validated (returned, not stored).
pub(crate) fn prepare_plan_source(plan: &mut NewPlan) -> Result<Option<GitContext>> {
    if plan.source_brief.as_deref() == Some(PlanRepo::INTAKE_MARKER) {
        return Err(CoreError::validation(format!(
            "source_brief `{}` is reserved for the legacy intake plan",
            PlanRepo::INTAKE_MARKER
        )));
    }
    if let Some(explicit) = plan.source_ref.take() {
        plan.source_ref = Some(normalized(&explicit)?);
    }
    plan.git_context
        .take()
        .map(GitContext::normalized)
        .transpose()
        .map_err(CoreError::validation)
}

impl CommandHandler {
    /// Resolve and check the plan's `source_ref` in place (ADR-0009). Runs
    /// under the handler's command lock, so the check, the auto-parent
    /// lookup and the plan creation are one atomic step.
    ///
    /// Order: explicit ref → parent plan's ref → policy `derive` over
    /// `git_context.branch` → policy `default_ref`. Only an explicit or
    /// derived ref auto-parents: an inherited one already has its parent,
    /// and a project-wide default would chain unrelated plans.
    pub(crate) async fn resolve_plan_source(
        &self,
        plan: &mut NewPlan,
    ) -> Result<Vec<MutationWarning>> {
        let git_context = prepare_plan_source(plan)?;
        let policy = match &self.project_settings {
            Some(settings) => match settings.intake_source(plan.project_id).await {
                Ok(policy) => policy.unwrap_or_default(),
                // A policy written by a newer server (unknown mode/field)
                // must not block intake: behave as if none were set.
                Err(CoreError::Serde(e)) => {
                    tracing::warn!(project_id = %plan.project_id, error = %e,
                        "unreadable intake_source policy; falling back to warn");
                    IntakeSourcePolicy::default()
                }
                Err(e) => return Err(e),
            },
            None => IntakeSourcePolicy::default(),
        };

        let mut may_auto_parent = plan.source_ref.is_some();
        if plan.source_ref.is_none() {
            if let (Some(parent_id), Some(plans)) = (plan.parent_plan_id, &self.plans) {
                plan.source_ref = plans.get(parent_id).await?.and_then(|p| p.source_ref);
            }
        }
        if plan.source_ref.is_none() {
            let branch = git_context.as_ref().and_then(|g| g.branch.as_deref());
            for rule in policy.derive.iter().filter(|r| r.from == "branch") {
                let Some(branch) = branch else { break };
                let derived = compile(&rule.pattern, "derive.pattern")?
                    .captures(branch)
                    .and_then(|caps| expand(&rule.ref_template, &caps));
                if let Some(derived) = derived {
                    plan.source_ref = Some(normalized(&derived)?);
                    may_auto_parent = true;
                    break;
                }
            }
        }
        if plan.source_ref.is_none() {
            if let Some(default_ref) = &policy.default_ref {
                plan.source_ref = Some(normalized(default_ref)?);
            }
        }

        let mut warnings = Vec::new();
        if let (Some(source_ref), None, Some(plans), true) = (
            &plan.source_ref,
            plan.parent_plan_id,
            &self.plans,
            may_auto_parent,
        ) {
            if let Some(root) = plans
                .earliest_by_source_ref(plan.project_id, source_ref)
                .await?
            {
                // The new plan has no id yet, so only the depth limit can trip.
                detect_parent_cycle(plans.as_ref(), PlanId::new(), root)
                    .await
                    .map_err(|e| {
                        CoreError::validation(format!(
                            "auto-parent to plan {root} (earliest plan with source `{source_ref}`) failed: {e}; pass an explicit parent_plan_id"
                        ))
                    })?;
                plan.parent_plan_id = Some(root);
                warnings.push(MutationWarning {
                    code: "plan_auto_parented".into(),
                    message: format!(
                        "a plan with source `{source_ref}` already exists; this plan was attached to it as a child"
                    ),
                    details: json!({ "parent_plan_id": root, "source_ref": source_ref }),
                });
            }
        }

        if policy.mode == IntakeSourceMode::Off {
            return Ok(warnings);
        }
        let problem = match &plan.source_ref {
            None => Some((
                "plan_source_missing",
                "plan has no source: pass `source.ref` (MCP tools) / `plan.source_ref` (HTTP), a URI such as an issue URL"
                    .to_string(),
            )),
            Some(source_ref) => {
                let mut matched = None;
                for channel in &policy.channels {
                    if channel_matches(channel, source_ref)? {
                        matched = Some(channel);
                        break;
                    }
                }
                let brief_empty = !plan
                    .source_brief
                    .as_deref()
                    .is_some_and(|b| !b.trim().is_empty());
                if !policy.channels.is_empty() && matched.is_none() {
                    Some((
                        "plan_source_channel_mismatch",
                        format!("source `{source_ref}` matches none of the project's channels"),
                    ))
                } else if matched.is_some_and(|c| c.note_required) && brief_empty {
                    Some((
                        "plan_source_note_required",
                        format!(
                            "the channel of source `{source_ref}` requires a note: pass `source_brief`"
                        ),
                    ))
                } else {
                    None
                }
            }
        };
        let Some((code, message)) = problem else {
            return Ok(warnings);
        };
        let channels: Vec<Value> = policy
            .channels
            .iter()
            .map(|c| json!({ "scheme": c.scheme, "pattern": c.pattern, "label": c.label }))
            .collect();
        if policy.mode == IntakeSourceMode::Enforce {
            return Err(CoreError::unprocessable(
                "plan_source_required",
                message,
                json!({ "reason": code, "channels": channels }),
            ));
        }
        warnings.push(MutationWarning {
            code: code.into(),
            message,
            details: json!({ "channels": channels }),
        });
        Ok(warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_substitutes_single_digit_groups() {
        let re = Regex::new(r"^(\d+)_(\w+)(-x)?").unwrap();
        let caps = re.captures("11084_av_x").unwrap();
        assert_eq!(
            expand("i/$1/$2/$$", &caps).as_deref(),
            Some("i/11084/av_x/$$")
        );
        assert_eq!(expand("a$0b$", &caps).as_deref(), Some("a$0b$"));
        // Group 3 did not participate, group 4 does not exist: no fire.
        assert_eq!(expand("i/$3", &caps), None);
        assert_eq!(expand("i/$4", &caps), None);
    }
}
