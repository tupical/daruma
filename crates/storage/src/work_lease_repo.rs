//! WorkLease repository — TTL'd, overlap-exclusive file/path reservations.
//!
//! Two agents closing tasks in parallel must not edit the same files. A lease
//! records the repo-relative path globs an agent is touching; [`try_reserve`]
//! grants them only when none overlap a *different* agent's live lease. Unlike
//! claims (a single-statement CAS), overlap is glob logic, so reservation runs
//! inside a `BEGIN IMMEDIATE` transaction that serializes concurrent reservers.
//!
//! [`try_reserve`]: WorkLeaseRepo::try_reserve

use chrono::{DateTime, Duration, Utc};
use sqlx::{Row, SqlitePool};
use taskagent_domain::WorkLease;
use taskagent_shared::{
    normalize_lease_path, paths_overlap, AgentId, CoreError, ProjectId, Result, TaskId, WorkLeaseId,
};

/// Outcome of an atomic [`WorkLeaseRepo::try_reserve`] attempt.
#[derive(Debug, Clone)]
pub enum ReserveOutcome {
    /// All requested paths were reserved (the freshly written leases).
    Reserved { leases: Vec<WorkLease> },
    /// A requested path overlaps a live lease held by another agent. Carries the
    /// holder + the task they're working so the requester can negotiate (e.g.
    /// `taskagent_signal_send`) or back off to a different task.
    Conflict {
        path: String,
        holder: AgentId,
        holder_task: TaskId,
    },
}

/// Read/write access to the `work_leases` table.
pub struct WorkLeaseRepo {
    pub(crate) pool: SqlitePool,
}

impl WorkLeaseRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Atomically reserve `paths` for `(agent_id, task_id)` for `ttl`.
    ///
    /// Conflicts only with *other* agents' live leases in the same project
    /// scope. Re-reserving the same path by the same agent refreshes its TTL;
    /// reserving new paths adds to the agent's set (predeclare-then-extend).
    pub async fn try_reserve(
        &self,
        agent_id: AgentId,
        task_id: TaskId,
        project_id: Option<ProjectId>,
        paths: Vec<String>,
        ttl: Duration,
    ) -> Result<ReserveOutcome> {
        let now = Utc::now();
        let now_s = now.to_rfc3339();
        let expires_at = now + ttl;
        let expires_s = expires_at.to_rfc3339();

        // Normalize + dedup the requested paths.
        let mut requested: Vec<String> = paths
            .iter()
            .map(|p| normalize_lease_path(p))
            .collect::<Vec<_>>();
        requested.sort();
        requested.dedup();

        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        // BEGIN IMMEDIATE takes the write lock up front so two concurrent
        // reservers serialize instead of deadlocking on lock upgrade.
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *conn)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        // Run the body; on any error roll back before returning.
        let result = async {
            // Live leases held by other agents in the same project scope.
            let select_sql = match project_id {
                Some(_) => {
                    "SELECT agent_id, task_id, path_glob FROM work_leases \
                     WHERE expires_at >= ? AND agent_id <> ? AND project_id = ?"
                }
                None => {
                    "SELECT agent_id, task_id, path_glob FROM work_leases \
                     WHERE expires_at >= ? AND agent_id <> ? AND project_id IS NULL"
                }
            };
            let mut q = sqlx::query(select_sql)
                .bind(&now_s)
                .bind(agent_id.to_string());
            if let Some(p) = &project_id {
                q = q.bind(p.to_string());
            }
            let existing = q
                .fetch_all(&mut *conn)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;

            for row in &existing {
                let holder_s: String = row
                    .try_get("agent_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let holder_task_s: String = row
                    .try_get("task_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let held: String = row
                    .try_get("path_glob")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                for req in &requested {
                    if paths_overlap(req, &held) {
                        let holder = holder_s
                            .parse::<AgentId>()
                            .map_err(|e| CoreError::serde(e.to_string()))?;
                        let holder_task = holder_task_s
                            .parse::<TaskId>()
                            .map_err(|e| CoreError::serde(e.to_string()))?;
                        return Ok(ReserveOutcome::Conflict {
                            path: req.clone(),
                            holder,
                            holder_task,
                        });
                    }
                }
            }

            // No conflict — upsert each requested path (refresh TTL / add new).
            let mut leases = Vec::with_capacity(requested.len());
            for req in &requested {
                sqlx::query(
                    "DELETE FROM work_leases WHERE agent_id = ? AND task_id = ? AND path_glob = ?",
                )
                .bind(agent_id.to_string())
                .bind(task_id.to_string())
                .bind(req)
                .execute(&mut *conn)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;

                let id = WorkLeaseId::new();
                sqlx::query(
                    "INSERT INTO work_leases \
                     (id, agent_id, task_id, project_id, path_glob, acquired_at, expires_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(id.to_string())
                .bind(agent_id.to_string())
                .bind(task_id.to_string())
                .bind(project_id.as_ref().map(|p| p.to_string()))
                .bind(req)
                .bind(&now_s)
                .bind(&expires_s)
                .execute(&mut *conn)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;

                leases.push(WorkLease {
                    id,
                    agent_id,
                    task_id,
                    project_id,
                    path_glob: req.clone(),
                    acquired_at: now,
                    expires_at,
                });
            }
            Ok(ReserveOutcome::Reserved { leases })
        }
        .await;

        match &result {
            Ok(_) => {
                sqlx::query("COMMIT")
                    .execute(&mut *conn)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }
            Err(_) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            }
        }
        result
    }

    /// Idempotently insert a lease row (used by `apply_event` on replay).
    pub async fn apply_reserved(&self, lease: &WorkLease) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO work_leases \
             (id, agent_id, task_id, project_id, path_glob, acquired_at, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(lease.id.to_string())
        .bind(lease.agent_id.to_string())
        .bind(lease.task_id.to_string())
        .bind(lease.project_id.as_ref().map(|p| p.to_string()))
        .bind(&lease.path_glob)
        .bind(lease.acquired_at.to_rfc3339())
        .bind(lease.expires_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Release **all** leases on a task, regardless of holder. Used to
    /// auto-clean leases when a task closes.
    pub async fn release_all_for_task(&self, task_id: TaskId) -> Result<()> {
        sqlx::query("DELETE FROM work_leases WHERE task_id = ?")
            .bind(task_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Release every lease held by `agent_id` on `task_id`.
    pub async fn release_for_task(&self, agent_id: AgentId, task_id: TaskId) -> Result<()> {
        sqlx::query("DELETE FROM work_leases WHERE agent_id = ? AND task_id = ?")
            .bind(agent_id.to_string())
            .bind(task_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Delete all expired leases, returning the distinct `(agent_id, task_id)`
    /// pairs that lost leases so callers can emit `FilesReleased` events.
    pub async fn sweep_expired(&self) -> Result<Vec<(AgentId, TaskId)>> {
        let now = Utc::now().to_rfc3339();
        let rows =
            sqlx::query("SELECT DISTINCT agent_id, task_id FROM work_leases WHERE expires_at < ?")
                .bind(&now)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;

        let pairs = rows
            .iter()
            .map(|r| {
                let a: String = r
                    .try_get("agent_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let t: String = r
                    .try_get("task_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok((
                    a.parse::<AgentId>()
                        .map_err(|e| CoreError::serde(e.to_string()))?,
                    t.parse::<TaskId>()
                        .map_err(|e| CoreError::serde(e.to_string()))?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;

        if !pairs.is_empty() {
            sqlx::query("DELETE FROM work_leases WHERE expires_at < ?")
                .bind(&now)
                .execute(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
        }
        Ok(pairs)
    }

    /// List active (non-expired) leases, optionally scoped to a project — the
    /// "backlog of active work with affected files".
    pub async fn list_active(&self, project_id: Option<ProjectId>) -> Result<Vec<WorkLease>> {
        let now = Utc::now().to_rfc3339();
        let rows =
            match &project_id {
                Some(p) => sqlx::query(
                    "SELECT id, agent_id, task_id, project_id, path_glob, acquired_at, expires_at \
                 FROM work_leases WHERE expires_at >= ? AND project_id = ? \
                 ORDER BY acquired_at",
                )
                .bind(&now)
                .bind(p.to_string())
                .fetch_all(&self.pool)
                .await,
                None => sqlx::query(
                    "SELECT id, agent_id, task_id, project_id, path_glob, acquired_at, expires_at \
                 FROM work_leases WHERE expires_at >= ? ORDER BY acquired_at",
                )
                .bind(&now)
                .fetch_all(&self.pool)
                .await,
            }
            .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_lease).collect()
    }
}

fn row_to_lease(r: &sqlx::sqlite::SqliteRow) -> Result<WorkLease> {
    let id: String = r
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let agent_id: String = r
        .try_get("agent_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let task_id: String = r
        .try_get("task_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let project_id: Option<String> = r
        .try_get("project_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let path_glob: String = r
        .try_get("path_glob")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let acquired_at: String = r
        .try_get("acquired_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let expires_at: String = r
        .try_get("expires_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    Ok(WorkLease {
        id: id
            .parse()
            .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?,
        agent_id: agent_id
            .parse()
            .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?,
        task_id: task_id
            .parse()
            .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?,
        project_id: project_id
            .map(|s| s.parse())
            .transpose()
            .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?,
        path_glob,
        acquired_at: parse_ts(&acquired_at)?,
        expires_at: parse_ts(&expires_at)?,
    })
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| CoreError::serde(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    async fn make_repo() -> (Db, WorkLeaseRepo) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = WorkLeaseRepo::new(db.pool().clone());
        (db, repo)
    }

    #[tokio::test]
    async fn reserve_then_conflicting_overlap_is_rejected() {
        let (_db, repo) = make_repo().await;
        let proj = ProjectId::new();
        let a1 = AgentId::new();
        let a2 = AgentId::new();
        let t1 = TaskId::new();
        let t2 = TaskId::new();

        let out = repo
            .try_reserve(
                a1,
                t1,
                Some(proj.clone()),
                vec!["crates/storage/src".into()],
                Duration::seconds(60),
            )
            .await
            .unwrap();
        assert!(matches!(out, ReserveOutcome::Reserved { .. }));

        // Overlapping descendant by another agent → Conflict with holder.
        let out2 = repo
            .try_reserve(
                a2,
                t2,
                Some(proj.clone()),
                vec!["crates/storage/src/claim_repo.rs".into()],
                Duration::seconds(60),
            )
            .await
            .unwrap();
        match out2 {
            ReserveOutcome::Conflict { holder, .. } => assert_eq!(holder, a1),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_overlapping_paths_both_reserve() {
        let (_db, repo) = make_repo().await;
        let proj = ProjectId::new();
        let a1 = AgentId::new();
        let a2 = AgentId::new();

        let r1 = repo
            .try_reserve(
                a1,
                TaskId::new(),
                Some(proj.clone()),
                vec!["crates/storage".into()],
                Duration::seconds(60),
            )
            .await
            .unwrap();
        let r2 = repo
            .try_reserve(
                a2,
                TaskId::new(),
                Some(proj.clone()),
                vec!["crates/core".into()],
                Duration::seconds(60),
            )
            .await
            .unwrap();
        assert!(matches!(r1, ReserveOutcome::Reserved { .. }));
        assert!(matches!(r2, ReserveOutcome::Reserved { .. }));
        assert_eq!(repo.list_active(Some(proj)).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn release_for_task_drops_leases() {
        let (_db, repo) = make_repo().await;
        let proj = ProjectId::new();
        let agent = AgentId::new();
        let task = TaskId::new();
        repo.try_reserve(
            agent,
            task,
            Some(proj.clone()),
            vec!["a".into(), "b".into()],
            Duration::seconds(60),
        )
        .await
        .unwrap();
        assert_eq!(repo.list_active(Some(proj.clone())).await.unwrap().len(), 2);

        repo.release_for_task(agent, task).await.unwrap();
        assert_eq!(repo.list_active(Some(proj)).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn same_agent_refresh_and_extend_does_not_self_conflict() {
        let (_db, repo) = make_repo().await;
        let proj = ProjectId::new();
        let agent = AgentId::new();
        let task = TaskId::new();

        repo.try_reserve(
            agent,
            task,
            Some(proj.clone()),
            vec!["src/a".into()],
            Duration::seconds(60),
        )
        .await
        .unwrap();
        // Re-reserve same path (refresh) + add a new one — must not conflict.
        let out = repo
            .try_reserve(
                agent,
                task,
                Some(proj.clone()),
                vec!["src/a".into(), "src/b".into()],
                Duration::seconds(120),
            )
            .await
            .unwrap();
        assert!(matches!(out, ReserveOutcome::Reserved { .. }));
        assert_eq!(repo.list_active(Some(proj)).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn sweep_expired_returns_pairs_and_clears() {
        let (db, repo) = make_repo().await;
        let agent = AgentId::new();
        let task = TaskId::new();
        sqlx::query(
            "INSERT INTO work_leases \
             (id, agent_id, task_id, project_id, path_glob, acquired_at, expires_at) \
             VALUES (?, ?, ?, NULL, ?, ?, ?)",
        )
        .bind(WorkLeaseId::new().to_string())
        .bind(agent.to_string())
        .bind(task.to_string())
        .bind("src/x")
        .bind(Utc::now().to_rfc3339())
        .bind("2000-01-01T00:00:00+00:00")
        .execute(db.pool())
        .await
        .unwrap();

        let released = repo.sweep_expired().await.unwrap();
        assert_eq!(released, vec![(agent, task)]);
        assert_eq!(repo.list_active(None).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn different_projects_do_not_conflict() {
        let (_db, repo) = make_repo().await;
        let p1 = ProjectId::new();
        let p2 = ProjectId::new();
        let a1 = AgentId::new();
        let a2 = AgentId::new();
        repo.try_reserve(
            a1,
            TaskId::new(),
            Some(p1),
            vec!["src".into()],
            Duration::seconds(60),
        )
        .await
        .unwrap();
        let out = repo
            .try_reserve(
                a2,
                TaskId::new(),
                Some(p2),
                vec!["src".into()],
                Duration::seconds(60),
            )
            .await
            .unwrap();
        assert!(matches!(out, ReserveOutcome::Reserved { .. }));
    }
}
