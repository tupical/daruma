//! Plan source umbrella at intake (ADR-0009): resolve `source_ref`, check it
//! against the project's `intake_source` policy, auto-parent repeat sources.

use std::collections::{HashMap, HashSet};

use daruma_api_dto::MutationWarning;
use daruma_domain::{
    normalize_source_ref, Actor, GitContext, IntakeSourceMode, IntakeSourcePolicy, NewPlan,
    SourceChannel, SourceInput, SourceNode, SOURCE_CHAIN_MAX,
};
use daruma_events::Event;
use daruma_shared::{time, CoreError, PlanId, ProjectId, Result};
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

fn chain_invalid(message: String) -> CoreError {
    CoreError::unprocessable("source_chain_invalid", message, Value::Null)
}

/// Validate a client chain — `head`, its `upstream`, then `more` — into
/// nodes, nearest first. Nodes without a ref get a synthetic `note:<uuid v7>`
/// here, so the event carries it and replay needs nothing else.
pub(crate) fn source_nodes(
    head: Option<&SourceInput>,
    more: &[SourceInput],
    actor: &Actor,
) -> Result<Vec<SourceNode>> {
    let inputs: Vec<&SourceInput> = head
        .into_iter()
        .chain(head.into_iter().flat_map(|h| h.upstream.iter()))
        .chain(more)
        .collect();
    if inputs.len() > SOURCE_CHAIN_MAX {
        return Err(chain_invalid(format!(
            "a source chain holds at most {SOURCE_CHAIN_MAX} nodes"
        )));
    }
    let now = time::now();
    inputs
        .iter()
        .enumerate()
        .map(|(i, input)| {
            if (i > 0 || head.is_none()) && !input.upstream.is_empty() {
                return Err(CoreError::validation(
                    "only the nearest source node may carry `upstream`",
                ));
            }
            input
                .to_node(|| format!("note:{}", uuid::Uuid::now_v7()), now, actor)
                .map_err(CoreError::validation)
        })
        .collect()
}

/// Shape checks every plan-creating command gets, with or without policy:
/// the legacy-intake marker is reserved for storage, an explicit ref is
/// normalised, `plan.source` becomes chain nodes (its ref is the explicit
/// `source_ref`), and `git_context` is validated (returned, not stored).
pub(crate) fn prepare_plan_source(
    plan: &mut NewPlan,
    actor: &Actor,
) -> Result<(Option<GitContext>, Vec<SourceNode>)> {
    if plan.source_brief.as_deref() == Some(PlanRepo::INTAKE_MARKER) {
        return Err(CoreError::validation(format!(
            "source_brief `{}` is reserved for the legacy intake plan",
            PlanRepo::INTAKE_MARKER
        )));
    }
    if let Some(explicit) = plan.source_ref.take() {
        plan.source_ref = Some(normalized(&explicit)?);
    }
    let nodes = match plan.source.take() {
        Some(source) => source_nodes(Some(&source), &[], actor)?,
        None => Vec::new(),
    };
    if let Some(nearest) = nodes.first() {
        match &plan.source_ref {
            Some(explicit) if *explicit != nearest.source_ref => {
                return Err(CoreError::validation(format!(
                    "`source.ref` `{}` differs from `source_ref` `{explicit}`; pass one",
                    nearest.source_ref
                )))
            }
            _ => plan.source_ref = Some(nearest.source_ref.clone()),
        }
    }
    let git_context = plan
        .git_context
        .take()
        .map(GitContext::normalized)
        .transpose()
        .map_err(CoreError::validation)?;
    Ok((git_context, nodes))
}

impl CommandHandler {
    async fn intake_policy(&self, project_id: ProjectId) -> Result<IntakeSourcePolicy> {
        Ok(match &self.project_settings {
            Some(settings) => match settings.intake_source(project_id).await {
                Ok(policy) => policy.unwrap_or_default(),
                // A policy written by a newer server (unknown mode/field)
                // must not block intake: behave as if none were set.
                Err(CoreError::Serde(e)) => {
                    tracing::warn!(%project_id, error = %e,
                        "unreadable intake_source policy; falling back to warn");
                    IntakeSourcePolicy::default()
                }
                Err(e) => return Err(e),
            },
            None => IntakeSourcePolicy::default(),
        })
    }

    /// Resolve and check the plan's source (ADR-0009) and build the
    /// `SourceUpserted` events that ride ahead of `PlanCreated`. Runs under
    /// the handler's command lock, so the check, the auto-parent lookup, the
    /// node upserts and the plan creation are one atomic step; a refusal
    /// (enforce 422, link conflict) writes nothing.
    pub(crate) async fn resolve_plan_source(
        &self,
        plan: &mut NewPlan,
        actor: &Actor,
    ) -> Result<(Vec<MutationWarning>, Vec<Event>)> {
        let (git_context, nodes) = prepare_plan_source(plan, actor)?;
        let mut warnings = self.apply_source_policy(plan, git_context).await?;
        let (events, kept) = self.plan_source_events(plan, nodes, actor).await?;
        warnings.extend(kept);
        Ok((warnings, events))
    }

    /// Nodes to write for a new plan: the client chain, or — when the ref
    /// came from elsewhere (parent, derive, default, bare `source_ref`) — a
    /// ref-only node. Intake never fails on a link conflict: the stored link
    /// wins (see [`Self::link_sources`]).
    pub(crate) async fn plan_source_events(
        &self,
        plan: &NewPlan,
        mut nodes: Vec<SourceNode>,
        actor: &Actor,
    ) -> Result<(Vec<Event>, Vec<MutationWarning>)> {
        if nodes.is_empty() {
            let Some(source_ref) = &plan.source_ref else {
                return Ok(Default::default());
            };
            nodes.push(
                SourceInput {
                    source_ref: Some(source_ref.clone()),
                    ..SourceInput::default()
                }
                .to_node(String::new, time::now(), actor)
                .map_err(CoreError::validation)?,
            );
        }
        self.link_sources(nodes, true).await
    }

    /// Link `nodes` (nearest first) into a chain: each node's upstream is
    /// the next one. Fill-only against stored nodes: a set link is never
    /// rewritten. On a different set link, `keep_existing` (intake) keeps it,
    /// drops the incoming tail above that node — a label-only tail node's
    /// fresh `note:` included — and warns `source_upstream_kept`; otherwise
    /// (explicit extend) it is a 409. The chain through the first node must
    /// stay acyclic and within [`SOURCE_CHAIN_MAX`] nodes (422). Returns the
    /// `SourceUpserted` events for nodes that change.
    pub(crate) async fn link_sources(
        &self,
        mut nodes: Vec<SourceNode>,
        keep_existing: bool,
    ) -> Result<(Vec<Event>, Vec<MutationWarning>)> {
        let Some(plans) = &self.plans else {
            return Ok(Default::default());
        };
        for i in 1..nodes.len() {
            nodes[i - 1].upstream_ref = Some(nodes[i].source_ref.clone());
        }
        let mut merged: HashMap<String, SourceNode> = HashMap::new();
        let mut events = Vec::new();
        let mut warnings = Vec::new();
        for node in &nodes {
            let stored = match merged.get(&node.source_ref) {
                Some(n) => Some(n.clone()),
                None => plans.get_source(&node.source_ref).await?,
            };
            let mut kept = None;
            let next = match &stored {
                Some(stored) => match stored.fill_from(node) {
                    Ok(next) => next,
                    Err(have) if keep_existing => {
                        kept = Some(have.clone());
                        stored
                            .fill_from(&SourceNode {
                                upstream_ref: Some(have),
                                ..node.clone()
                            })
                            .expect("same upstream never conflicts")
                    }
                    Err(have) => {
                        return Err(CoreError::coded_conflict(
                            "source_upstream_conflict",
                            format!(
                                "source `{}` already has upstream `{have}`; a set link is never rewritten",
                                node.source_ref
                            ),
                        ))
                    }
                },
                None => node.clone(),
            };
            if stored.as_ref() != Some(&next) {
                merged.insert(next.source_ref.clone(), next.clone());
                events.push(Event::SourceUpserted { source: next });
            }
            if let Some(kept) = kept {
                warnings.push(MutationWarning {
                    code: "source_upstream_kept".into(),
                    message: format!(
                        "source `{}` already has upstream `{kept}`; kept it and dropped the sources given above it",
                        node.source_ref
                    ),
                    details: json!({
                        "ref": node.source_ref,
                        "kept_upstream": kept,
                        "dropped_upstream": node.upstream_ref,
                    }),
                });
                break;
            }
        }

        let Some(first) = nodes.first() else {
            return Ok((events, warnings));
        };
        // ponytail: the depth check counts the longest stored chain below the
        // first node plus the walk above it; a per-plan check if that ever
        // proves too coarse.
        let (_, below) = plans.source_descendants(&first.source_ref).await?;
        let mut seen = HashSet::new();
        let mut cursor = Some(first.source_ref.clone());
        while let Some(current) = cursor {
            if !seen.insert(current.clone()) {
                return Err(chain_invalid(format!(
                    "linking would close a cycle at source `{current}`"
                )));
            }
            if below + seen.len() > SOURCE_CHAIN_MAX {
                return Err(chain_invalid(format!(
                    "a source chain holds at most {SOURCE_CHAIN_MAX} nodes"
                )));
            }
            cursor = match merged.get(&current) {
                Some(node) => node.upstream_ref.clone(),
                None => plans
                    .get_source(&current)
                    .await?
                    .and_then(|n| n.upstream_ref),
            };
        }
        Ok((events, warnings))
    }

    /// `ExtendSource` (ADR-0009): give a source-less plan its nearest node
    /// (`PlanSourceSet`, checked against the plan project's channels), or
    /// attach `upstream` to the top node of an existing chain.
    pub(crate) async fn extend_source(
        &self,
        plan_id: Option<PlanId>,
        source_ref: Option<&str>,
        source: Option<&SourceInput>,
        upstream: &[SourceInput],
        actor: &Actor,
    ) -> Result<(Vec<MutationWarning>, Vec<Event>)> {
        let plans = self
            .plans
            .as_ref()
            .ok_or_else(|| CoreError::storage("plan repository not configured"))?;
        let start = match (plan_id, source_ref) {
            (Some(id), None) => {
                let plan = plans
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("plan {id}")))?;
                match (plan.source_ref, source) {
                    (None, Some(source)) => {
                        let nodes = source_nodes(Some(source), upstream, actor)?;
                        let nearest = nodes[0].source_ref.clone();
                        let policy = self.intake_policy(plan.project_id).await?;
                        let warnings = source_policy_findings(
                            &policy,
                            Some(&nearest),
                            plan.source_brief.as_deref(),
                        )?;
                        let (mut events, _) = self.link_sources(nodes, false).await?;
                        events.push(Event::PlanSourceSet {
                            plan_id: id,
                            source_ref: nearest,
                            at: time::now(),
                        });
                        return Ok((warnings, events));
                    }
                    (None, None) => {
                        return Err(CoreError::validation(format!(
                            "plan {id} has no source yet: pass `source`"
                        )))
                    }
                    (Some(set), Some(_)) => {
                        return Err(CoreError::coded_conflict(
                            "plan_source_already_set",
                            format!(
                                "plan {id} already has source `{set}`; extend it with `upstream`"
                            ),
                        ))
                    }
                    (Some(set), None) => set,
                }
            }
            (None, Some(raw)) => {
                if source.is_some() {
                    return Err(CoreError::validation(
                        "`source` sets a plan's first source: pass `plan_id`, or `upstream` to extend `ref`",
                    ));
                }
                let start = normalized(raw)?;
                if plans.get_source(&start).await?.is_none() {
                    return Err(CoreError::not_found(format!("source `{start}`")));
                }
                start
            }
            _ => {
                return Err(CoreError::validation(
                    "pass exactly one of `plan_id` / `ref`",
                ))
            }
        };
        if upstream.is_empty() {
            return Err(CoreError::validation(
                "`upstream` (nearest first) is required to extend a chain",
            ));
        }
        // The top node: the first without `upstream_ref`, walked from start.
        let mut top = start;
        for _ in 0..SOURCE_CHAIN_MAX {
            match plans.get_source(&top).await?.and_then(|n| n.upstream_ref) {
                Some(up) => top = up,
                None => break,
            }
        }
        let mut nodes = vec![SourceInput {
            source_ref: Some(top),
            ..SourceInput::default()
        }
        .to_node(String::new, time::now(), actor)
        .map_err(CoreError::validation)?];
        nodes.extend(source_nodes(None, upstream, actor)?);
        Ok((Vec::new(), self.link_sources(nodes, false).await?.0))
    }

    /// Order: explicit ref → parent plan's ref → policy `derive` over
    /// `git_context.branch` → policy `default_ref`. Only an explicit or
    /// derived ref auto-parents: an inherited one already has its parent,
    /// a project-wide default would chain unrelated plans, and a `note:`
    /// ref is unique to its node.
    async fn apply_source_policy(
        &self,
        plan: &mut NewPlan,
        git_context: Option<GitContext>,
    ) -> Result<Vec<MutationWarning>> {
        let policy = self.intake_policy(plan.project_id).await?;

        let mut may_auto_parent = plan
            .source_ref
            .as_deref()
            .is_some_and(|r| !r.starts_with("note:"));
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

        warnings.extend(source_policy_findings(
            &policy,
            plan.source_ref.as_deref(),
            plan.source_brief.as_deref(),
        )?);
        Ok(warnings)
    }
}

/// The policy verdict on a plan's nearest source: `Err` 422 in enforce, a
/// warning in warn, nothing in off or when the source passes.
pub(crate) fn source_policy_findings(
    policy: &IntakeSourcePolicy,
    source_ref: Option<&str>,
    source_brief: Option<&str>,
) -> Result<Vec<MutationWarning>> {
    if policy.mode == IntakeSourceMode::Off {
        return Ok(Vec::new());
    }
    let problem = match source_ref {
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
                let brief_empty = !source_brief.is_some_and(|b| !b.trim().is_empty());
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
        return Ok(Vec::new());
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
    Ok(vec![MutationWarning {
        code: code.into(),
        message,
        details: json!({ "channels": channels }),
    }])
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
