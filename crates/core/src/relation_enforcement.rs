//! Enforcement helpers for typed task relations (§3.2).
//!
//! - [`detect_cycle`] — iterative DFS to reject Blocks-kind cycles.
//! - [`list_active_blockers`] — blockers whose status is not Done (W2.2 helper).
//! - [`list_downstream_to_unblock`] — downstream tasks that become unblocked (W2.2 helper).

use std::collections::HashSet;

use taskagent_domain::{RelationKind, Status};
use taskagent_shared::{CoreError, Result, TaskId};
use taskagent_storage::{RelationRepo, TaskRepo};

/// Maximum DFS depth before aborting with `relation_graph_too_deep`.
const MAX_DEPTH: usize = 1000;

/// Detect whether adding `from --[kind]--> to` would introduce a cycle.
///
/// Only `Blocks` relations form a DAG that must be kept acyclic.
/// For other kinds this is a no-op that always returns `Ok(())`.
///
/// Returns:
/// - `CoreError::validation("cycle_detected")` for a self-loop or reachable cycle.
/// - `CoreError::validation("relation_graph_too_deep")` when DFS exceeds `MAX_DEPTH`.
pub async fn detect_cycle(
    repo: &RelationRepo,
    from: TaskId,
    to: TaskId,
    kind: RelationKind,
) -> Result<()> {
    if kind != RelationKind::Blocks {
        return Ok(()); // only Blocks edges form the acyclic DAG
    }
    if from == to {
        return Err(CoreError::validation("cycle_detected"));
    }

    // Iterative DFS upward from `to`: if we can reach `from`, adding this
    // edge would create a cycle.
    let mut visited: HashSet<TaskId> = HashSet::new();
    let mut stack: Vec<TaskId> = vec![to];
    let mut steps: usize = 0;

    while let Some(curr) = stack.pop() {
        steps += 1;
        if steps > MAX_DEPTH {
            return Err(CoreError::validation("relation_graph_too_deep"));
        }
        if curr == from {
            return Err(CoreError::validation("cycle_detected"));
        }
        if !visited.insert(curr) {
            continue;
        }
        // Walk forward along existing Blocks edges from `curr`.
        let targets = repo.list_blocks_targets(curr).await?;
        stack.extend(targets.into_iter().map(|r| r.to));
    }

    Ok(())
}

/// Return blockers of `task_id` whose status is not `Done`.
///
/// Used by W2.2 to gate `SetStatus(Done)` and `CompleteTask`.
/// Implemented here; called from handler.rs in W2.2.
pub async fn list_active_blockers(
    repo: &RelationRepo,
    tasks: &TaskRepo,
    task_id: TaskId,
) -> Result<Vec<TaskId>> {
    let blockers = repo.list_blockers(task_id).await?;
    let mut active = Vec::new();
    for rel in blockers {
        if let Some(task) = tasks.get(rel.from).await? {
            if task.status != Status::Done {
                active.push(rel.from);
            }
        }
        // If the blocker task no longer exists, skip it (treat as resolved).
    }
    Ok(active)
}

/// Return downstream tasks of `blocker_id` that would become fully unblocked
/// if `blocker_id` transitions to Done.
///
/// A downstream task is "fully unblocked" when all of its remaining blockers
/// (other than `blocker_id` itself) are already Done.
///
/// Used by W2.2 to emit `TaskUnblocked` events alongside `TaskStatusChanged`.
/// Implemented here; called from handler.rs in W2.2.
pub async fn list_downstream_to_unblock(
    repo: &RelationRepo,
    tasks: &TaskRepo,
    blocker_id: TaskId,
) -> Result<Vec<TaskId>> {
    // Find all tasks that `blocker_id` blocks.
    let downstream = repo.list_blocks_targets(blocker_id).await?;
    let mut result = Vec::new();

    for rel in downstream {
        let target = rel.to;
        // Fetch all blockers of `target`.
        let all_blockers = repo.list_blockers(target).await?;
        // `target` is unblocked if every blocker other than `blocker_id` is Done.
        let mut all_done = true;
        for b in &all_blockers {
            if b.from == blocker_id {
                continue; // this is the one transitioning to Done right now
            }
            match tasks.get(b.from).await? {
                Some(t) if t.status == Status::Done => {}
                _ => {
                    all_done = false;
                    break;
                }
            }
        }
        if all_done {
            result.push(target);
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use taskagent_domain::{Actor, Relation};
    use taskagent_shared::{time, RelationId};
    use taskagent_storage::{Db, RelationRepo};

    async fn make_repo() -> RelationRepo {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        RelationRepo::new(db.pool().clone())
    }

    fn rel(from: TaskId, to: TaskId, kind: RelationKind) -> Relation {
        Relation {
            id: RelationId::new(),
            from,
            to,
            kind,
            created_at: time::now(),
            created_by: Actor::user(),
        }
    }

    #[tokio::test]
    async fn detect_cycle_self_loop() {
        let repo = make_repo().await;
        let a = TaskId::new();
        let err = detect_cycle(&repo, a, a, RelationKind::Blocks)
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Validation(msg) if msg.contains("cycle_detected")));
    }

    #[tokio::test]
    async fn detect_cycle_no_cycle_different_tasks() {
        let repo = make_repo().await;
        let a = TaskId::new();
        let b = TaskId::new();
        detect_cycle(&repo, a, b, RelationKind::Blocks)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn detect_cycle_chain() {
        // A blocks B, B blocks C — adding C blocks A → cycle
        let repo = make_repo().await;
        let a = TaskId::new();
        let b = TaskId::new();
        let c = TaskId::new();

        repo.insert(&rel(a, b, RelationKind::Blocks)).await.unwrap();
        repo.insert(&rel(b, c, RelationKind::Blocks)).await.unwrap();

        let err = detect_cycle(&repo, c, a, RelationKind::Blocks)
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Validation(msg) if msg.contains("cycle_detected")));
    }

    #[tokio::test]
    async fn detect_cycle_non_blocks_always_ok() {
        let repo = make_repo().await;
        let a = TaskId::new();
        // Self-loop in RelatesTo is OK
        detect_cycle(&repo, a, a, RelationKind::RelatesTo)
            .await
            .unwrap();
        detect_cycle(&repo, a, a, RelationKind::Duplicates)
            .await
            .unwrap();
    }
}
