//! Read-only helpers for plan execution graphs and task readiness.

use std::collections::{HashMap, HashSet};

use daruma_domain::{
    Actor, CanStart, CanStartBlocker, CanStartRule, PlanFanoutWave, PlanGraph, PlanGraphEdge,
    PlanGraphNode, RelationKind, Status, Task,
};
use daruma_events::Event;
use daruma_shared::{CoreError, PlanId, Result, TaskId};
use daruma_storage::{PlanRepo, RelationRepo, TaskRepo};

use crate::handler::blocked_outcomes;
use crate::lifecycle_gate::{derive_gate_checks, GateDecision, GateOverride, LifecycleGate};

pub async fn plan_graph(
    plans: &PlanRepo,
    tasks: &TaskRepo,
    relations: &RelationRepo,
    plan_id: PlanId,
) -> Result<PlanGraph> {
    ensure_plan_exists(plans, plan_id).await?;
    let plan_tasks = plans.list_tasks_ordered(plan_id).await?;
    let task_ids = plan_tasks.iter().map(|pt| pt.task_id).collect::<Vec<_>>();
    let plan_task_ids = plan_tasks
        .iter()
        .map(|pt| pt.task_id)
        .collect::<HashSet<_>>();
    let task_map = load_tasks(tasks, task_ids.iter().copied()).await?;

    let nodes = plan_tasks
        .iter()
        .filter_map(|pt| {
            task_map.get(&pt.task_id).map(|task| PlanGraphNode {
                task_id: pt.task_id,
                position: pt.position,
                depends_on: pt.depends_on.clone(),
                title: task.title.clone(),
                status: task.status,
            })
        })
        .collect::<Vec<_>>();

    let mut edges = Vec::new();
    for pt in &plan_tasks {
        for dep in &pt.depends_on {
            if plan_task_ids.contains(dep) {
                edges.push(PlanGraphEdge {
                    from: *dep,
                    to: pt.task_id,
                    kind: "depends_on".to_string(),
                });
            }
        }
    }

    for rel in relations.list_by_task_ids(&task_ids).await? {
        if rel.kind == RelationKind::Blocks
            && plan_task_ids.contains(&rel.from)
            && plan_task_ids.contains(&rel.to)
        {
            edges.push(PlanGraphEdge {
                from: rel.from,
                to: rel.to,
                kind: "blocks".to_string(),
            });
        }
    }

    Ok(PlanGraph { nodes, edges })
}

pub async fn plan_fanout(
    plans: &PlanRepo,
    tasks: &TaskRepo,
    relations: &RelationRepo,
    plan_id: PlanId,
) -> Result<Vec<PlanFanoutWave>> {
    ensure_plan_exists(plans, plan_id).await?;
    let plan_tasks = plans.list_tasks_ordered(plan_id).await?;
    let task_ids = plan_tasks.iter().map(|pt| pt.task_id).collect::<Vec<_>>();
    let task_map = load_tasks(tasks, task_ids.iter().copied()).await?;
    let is_done = |id: &TaskId| task_map.get(id).is_some_and(|t| t.status == Status::Done);
    let mut remaining = plan_tasks
        .iter()
        .filter_map(|pt| {
            task_map
                .get(&pt.task_id)
                .filter(|task| task.status != Status::Done)
                .map(|_| pt.task_id)
        })
        .collect::<HashSet<_>>();

    let mut incoming: HashMap<TaskId, HashSet<TaskId>> = HashMap::new();
    for pt in &plan_tasks {
        if !remaining.contains(&pt.task_id) {
            continue;
        }
        for dep in &pt.depends_on {
            if !is_done(dep) {
                incoming.entry(pt.task_id).or_default().insert(*dep);
            }
        }
    }

    let relations_list = relations.list_by_task_ids(&task_ids).await?;
    let mut blocked_by = HashSet::new();
    for rel in &relations_list {
        if rel.kind == RelationKind::Blocks && remaining.contains(&rel.to) {
            blocked_by.insert(rel.from);
        }
    }

    let blockers_map = load_tasks(tasks, blocked_by).await?;
    for rel in &relations_list {
        if rel.kind != RelationKind::Blocks || !remaining.contains(&rel.to) {
            continue;
        }
        let from_done = blockers_map
            .get(&rel.from)
            .is_some_and(|t| t.status == Status::Done);
        if !from_done {
            incoming.entry(rel.to).or_default().insert(rel.from);
        }
    }

    let mut waves = Vec::new();
    while !remaining.is_empty() {
        let ready = plan_tasks
            .iter()
            .map(|pt| pt.task_id)
            .filter(|task_id| remaining.contains(task_id))
            .filter(|task_id| incoming.get(task_id).map_or(true, HashSet::is_empty))
            .collect::<Vec<_>>();
        if ready.is_empty() {
            break;
        }

        for task_id in &ready {
            remaining.remove(task_id);
        }
        for blockers in incoming.values_mut() {
            for task_id in &ready {
                blockers.remove(task_id);
            }
        }

        waves.push(PlanFanoutWave {
            wave: waves.len() as u32,
            tasks: ready,
        });
    }

    Ok(waves)
}

/// Can this task be moved into `in_progress` right now?
///
/// Two different things, deliberately reported apart:
///
/// - `rule_blockers` — lifecycle rules that HARD-block the transition. Asking is
///   a dry run: `LifecycleGate` is read-only and deterministic by contract, so
///   it costs nothing but the query.
/// - `blockers` — relation blockers. Starting a task does NOT enforce these
///   (only `→ Done` does, see `CommandHandler`); they are a readiness policy the
///   caller is expected to respect, and `force` exists for the cases it should
///   not. So `ready == false` from relations alone does not mean the transition
///   would be refused.
///
/// The gate input is built by [`derive_gate_checks`] from the very event the
/// real transition would emit, rather than assembled by hand here. That is the
/// whole point: a hand-built `GateCheck` drifts from the real one, and then
/// `can_start` starts lying again, just more subtly.
///
/// `gate` is optional and carries its actor: with no gate wired the answer is
/// relation-only, exactly as before. The pair is one argument so a caller
/// cannot supply half of it.
pub async fn can_start(
    tasks: &TaskRepo,
    relations: &RelationRepo,
    gate: Option<(&dyn LifecycleGate, &Actor)>,
    task_id: TaskId,
) -> Result<CanStart> {
    let task = tasks
        .get(task_id)
        .await?
        .ok_or_else(|| CoreError::not_found(format!("task {task_id}")))?;

    let relations = relations.list_blockers(task_id).await?;
    let mut blockers = Vec::new();
    if !relations.is_empty() {
        let from_ids: Vec<TaskId> = relations.iter().map(|rel| rel.from).collect();
        let tasks_list = tasks.get_many(&from_ids).await?;
        for task in tasks_list {
            if task.status != Status::Done {
                blockers.push(CanStartBlocker {
                    task_id: task.id,
                    title: task.title,
                    status: task.status,
                });
            }
        }
    }

    let (rule_blockers, rule_warnings) = rule_readiness(gate, &task).await?;

    let ready = blockers.is_empty() && rule_blockers.is_empty();
    let reason = match (blockers.len(), rule_blockers.len()) {
        (0, 0) => "ready".to_string(),
        (0, r) => format!("blocked_by_{r}_rule(s)"),
        (t, 0) => format!("blocked_by_{t}_task(s)"),
        (t, r) => format!("blocked_by_{t}_task(s)_and_{r}_rule(s)"),
    };

    Ok(CanStart {
        ready,
        blockers,
        rule_blockers,
        rule_warnings,
        reason,
    })
}

/// Dry-run the lifecycle gate for `task` → `in_progress`.
///
/// Returns `(blocking rules, advisory rules)`. A `required` rule blocks;
/// a `recommendation` only warns and must never move `ready`, or `can_start`
/// would acquire the mirror-image defect — not-ready for a transition that
/// actually succeeds.
async fn rule_readiness(
    gate: Option<(&dyn LifecycleGate, &Actor)>,
    task: &Task,
) -> Result<(Vec<CanStartRule>, Vec<CanStartRule>)> {
    let Some((gate, actor)) = gate else {
        return Ok((Vec::new(), Vec::new()));
    };
    // A no-op transition emits no events and therefore passes no gate
    // (`CommandHandler::emit_status_transition_events` returns early when the
    // status already matches). Reporting rule blockers here would claim the
    // caller cannot do something that would in fact succeed.
    if task.status == Status::InProgress {
        return Ok((Vec::new(), Vec::new()));
    }

    let events = vec![Event::TaskStatusChanged {
        task_id: task.id,
        from: task.status,
        to: Status::InProgress,
    }];

    let mut rule_blockers = Vec::new();
    let mut rule_warnings = Vec::new();
    for check in derive_gate_checks(&events) {
        // No override: the question is whether a NORMAL start works. A caller
        // holding `force` + `override_reason` can get past a rule that permits
        // it, so `ready == false` does not mean "impossible" — it means "not
        // without an explicit, recorded override".
        match gate.check(actor, &check, &GateOverride::default()).await? {
            GateDecision::Allowed => {}
            GateDecision::Warning(batch) => {
                rule_warnings.extend(batch.iter().map(|w| CanStartRule {
                    rule_key: warning_rule_key(w),
                    message: w.message.clone(),
                }));
            }
            GateDecision::Blocked { message, details } => {
                rule_blockers.extend(blocked_outcomes(&details, &message).into_iter().map(
                    |(outcome, message)| CanStartRule {
                        rule_key: rule_key_of(&outcome).unwrap_or_else(|| UNNAMED_RULE.to_string()),
                        message,
                    },
                ));
                // A blocked decision packs *every* acting rule into
                // `details.outcomes`, and `blocked_outcomes` keeps only the
                // blocking ones. Without this the advisories would vanish while
                // a blocker is present and reappear the moment it is satisfied
                // — the caller would think the list grew on its own.
                rule_warnings.extend(warning_outcomes(&details));
            }
        }
    }
    Ok((rule_blockers, rule_warnings))
}

/// Stand-in when a gate reports a rule without naming it: a stable literal
/// beats an empty string, which reads like a bug on the client side.
const UNNAMED_RULE: &str = "unnamed_rule";

fn rule_key_of(details: &serde_json::Value) -> Option<String> {
    details
        .get("rule_key")
        .and_then(|k| k.as_str())
        .map(str::to_string)
}

/// `MutationWarning::code` is `rule_warning:<key>` (see `rule_engine`), so the
/// prefix comes off before it stands in for a missing `rule_key`.
fn warning_rule_key(warning: &daruma_api_dto::MutationWarning) -> String {
    if let Some(key) = rule_key_of(&warning.details) {
        return key;
    }
    warning
        .code
        .strip_prefix("rule_warning:")
        .unwrap_or(&warning.code)
        .to_string()
}

/// Advisory rules carried inside a `Blocked` payload's structured outcomes.
fn warning_outcomes(details: &serde_json::Value) -> Vec<CanStartRule> {
    let Some(outcomes) = details.get("outcomes").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    outcomes
        .iter()
        .filter(|o| o.get("decision").and_then(|d| d.as_str()) == Some("warning"))
        .map(|o| CanStartRule {
            rule_key: rule_key_of(o).unwrap_or_else(|| UNNAMED_RULE.to_string()),
            message: o
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string(),
        })
        .collect()
}

async fn ensure_plan_exists(plans: &PlanRepo, plan_id: PlanId) -> Result<()> {
    plans
        .get(plan_id)
        .await?
        .ok_or_else(|| CoreError::not_found(format!("plan {plan_id}")))?;
    Ok(())
}

async fn load_tasks(
    tasks: &TaskRepo,
    task_ids: impl IntoIterator<Item = TaskId>,
) -> Result<HashMap<TaskId, daruma_domain::Task>> {
    let ids: Vec<TaskId> = task_ids.into_iter().collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let task_list = tasks.get_many(&ids).await?;
    let mut out = HashMap::new();
    for task in task_list {
        out.insert(task.id, task);
    }
    Ok(out)
}
