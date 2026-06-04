//! Read-only helpers for plan execution graphs and task readiness.

use std::collections::{HashMap, HashSet};

use taskagent_domain::{
    CanStart, CanStartBlocker, PlanFanoutWave, PlanGraph, PlanGraphEdge, PlanGraphNode,
    RelationKind, Status,
};
use taskagent_shared::{CoreError, PlanId, Result, TaskId};
use taskagent_storage::{PlanRepo, RelationRepo, TaskRepo};

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

pub async fn can_start(
    tasks: &TaskRepo,
    relations: &RelationRepo,
    task_id: TaskId,
) -> Result<CanStart> {
    tasks
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

    let ready = blockers.is_empty();
    let reason = if ready {
        "ready".to_string()
    } else {
        format!("blocked_by_{}_task(s)", blockers.len())
    };

    Ok(CanStart {
        ready,
        blockers,
        reason,
    })
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
) -> Result<HashMap<TaskId, taskagent_domain::Task>> {
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
