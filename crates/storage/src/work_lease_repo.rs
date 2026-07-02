//! WorkLease repository — TTL'd, overlap-exclusive file/path reservations.
//!
//! Two agents closing tasks in parallel must not edit the same files. A lease
//! records the repo-relative path globs an agent is touching; [`try_reserve`]
//! grants them only when none overlap a *different* agent's live lease. Unlike
//! claims (a single-statement CAS), overlap is glob logic, so reservation runs
//! inside a `BEGIN IMMEDIATE` transaction that serializes concurrent reservers.
//!
//! [`try_reserve`]: WorkLeaseRepo::try_reserve

use crate::parse_ts;
use chrono::{Duration, Utc};
use sqlx::{Row, SqlitePool};
use daruma_domain::{canonical_target_uri, targets_overlap, LeaseMode, WorkLease};
use daruma_shared::{AgentId, CoreError, ProjectId, Result, TaskId, WorkLeaseId};

/// Outcome of an atomic [`WorkLeaseRepo::try_reserve`] attempt.
#[derive(Debug, Clone)]
pub enum ReserveOutcome {
    /// All requested paths were reserved (the freshly written leases).
    Reserved { leases: Vec<WorkLease> },
    /// A requested path overlaps a live lease held by another agent. Carries the
    /// holder + the task they're working so the requester can negotiate (e.g.
    /// `daruma_signal_send`) or back off to a different task.
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

    /// Atomically reserve `paths` (exclusive `file://` targets) — the
    /// pre-P1 surface, kept verbatim for existing callers and the
    /// `reserve_files` wire contract. Delegates to [`try_reserve_targets`].
    ///
    /// [`try_reserve_targets`]: WorkLeaseRepo::try_reserve_targets
    pub async fn try_reserve(
        &self,
        agent_id: AgentId,
        task_id: TaskId,
        project_id: Option<ProjectId>,
        paths: Vec<String>,
        ttl: Duration,
    ) -> Result<ReserveOutcome> {
        self.try_reserve_targets(
            agent_id,
            task_id,
            project_id,
            paths,
            LeaseMode::Exclusive,
            ttl,
        )
        .await
    }

    /// Atomically reserve resource `targets` for `(agent_id, task_id)` in
    /// `mode` for `ttl`. Targets are canonicalized (`file://` for bare
    /// paths; `artifact://`/`contract://`/`env://` exact-match).
    ///
    /// Conflicts only with *other* agents' live leases in the same project
    /// scope whose mode conflicts per [`LeaseMode::conflicts_with`]
    /// (`intent` never blocks; two `shared_read`/`review` coexist; any
    /// `exclusive` pairing conflicts). Re-reserving by the same agent
    /// refreshes the TTL and re-issues a fencing token; new targets extend
    /// the agent's set (predeclare-then-extend).
    ///
    /// Every granted lease carries a **fencing token** — a monotonic
    /// per-resource counter bumped inside the same `BEGIN IMMEDIATE`
    /// transaction — so a holder that lost its lease cannot commit writes
    /// with an outdated token (validate via [`check_fencing_token`]).
    ///
    /// [`check_fencing_token`]: WorkLeaseRepo::check_fencing_token
    pub async fn try_reserve_targets(
        &self,
        agent_id: AgentId,
        task_id: TaskId,
        project_id: Option<ProjectId>,
        targets: Vec<String>,
        mode: LeaseMode,
        ttl: Duration,
    ) -> Result<ReserveOutcome> {
        let now = Utc::now();
        let now_s = now.to_rfc3339();
        let expires_at = now + ttl;
        let expires_s = expires_at.to_rfc3339();

        // Canonicalize + dedup the requested targets.
        let mut requested: Vec<String> = targets
            .iter()
            .map(|t| canonical_target_uri(t))
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
                    "SELECT agent_id, task_id, path_glob, target_uri, mode FROM work_leases \
                     WHERE expires_at >= ? AND agent_id <> ? AND project_id = ?"
                }
                None => {
                    "SELECT agent_id, task_id, path_glob, target_uri, mode FROM work_leases \
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
                let held_glob: String = row
                    .try_get("path_glob")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let held_target: Option<String> = row
                    .try_get("target_uri")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let held_mode: String = row
                    .try_get("mode")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let held_mode = LeaseMode::parse(&held_mode).unwrap_or_default();
                if !mode.conflicts_with(held_mode) {
                    continue;
                }
                // Pre-0033 rows have no target_uri — treat as file://<glob>.
                let held = held_target.unwrap_or_else(|| canonical_target_uri(&held_glob));
                for req in &requested {
                    if targets_overlap(req, &held) {
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

            // No conflict — upsert each requested target (refresh TTL / add
            // new) and issue a fresh fencing token per resource. The token
            // bump shares the BEGIN IMMEDIATE transaction with the conflict
            // scan, so token order == grant order.
            let mut leases = Vec::with_capacity(requested.len());
            for req in &requested {
                // Legacy/glob projection of the target for pre-mode readers.
                let glob = req.strip_prefix("file://").unwrap_or(req).to_string();

                sqlx::query(
                    "DELETE FROM work_leases WHERE agent_id = ? AND task_id = ? \
                     AND (target_uri = ? OR (target_uri IS NULL AND path_glob = ?))",
                )
                .bind(agent_id.to_string())
                .bind(task_id.to_string())
                .bind(req)
                .bind(&glob)
                .execute(&mut *conn)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;

                sqlx::query(
                    "INSERT OR IGNORE INTO lease_fencing_seq (resource_key, seq) VALUES (?, 0)",
                )
                .bind(req)
                .execute(&mut *conn)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
                sqlx::query("UPDATE lease_fencing_seq SET seq = seq + 1 WHERE resource_key = ?")
                    .bind(req)
                    .execute(&mut *conn)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let token: i64 =
                    sqlx::query("SELECT seq FROM lease_fencing_seq WHERE resource_key = ?")
                        .bind(req)
                        .fetch_one(&mut *conn)
                        .await
                        .map_err(|e| CoreError::storage(e.to_string()))?
                        .try_get("seq")
                        .map_err(|e| CoreError::storage(e.to_string()))?;

                let id = WorkLeaseId::new();
                sqlx::query(
                    "INSERT INTO work_leases \
                     (id, agent_id, task_id, project_id, path_glob, target_uri, mode, \
                      fencing_token, acquired_at, expires_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(id.to_string())
                .bind(agent_id.to_string())
                .bind(task_id.to_string())
                .bind(project_id.as_ref().map(|p| p.to_string()))
                .bind(&glob)
                .bind(req)
                .bind(mode.as_str())
                .bind(token)
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
                    path_glob: glob,
                    target_uri: Some(req.clone()),
                    mode,
                    fencing_token: Some(token),
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
             (id, agent_id, task_id, project_id, path_glob, target_uri, mode, \
              fencing_token, acquired_at, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(lease.id.to_string())
        .bind(lease.agent_id.to_string())
        .bind(lease.task_id.to_string())
        .bind(lease.project_id.as_ref().map(|p| p.to_string()))
        .bind(&lease.path_glob)
        .bind(lease.target_uri.as_deref())
        .bind(lease.mode.as_str())
        .bind(lease.fencing_token)
        .bind(lease.acquired_at.to_rfc3339())
        .bind(lease.expires_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Fencing-token validation for resource writes: `Ok(true)` iff `token`
    /// is the resource's *current* sequence value AND `agent_id` still holds
    /// a live lease on the resource carrying that token. A holder whose
    /// lease expired (or was superseded by a newer grant) gets `Ok(false)`
    /// and must re-reserve before writing.
    pub async fn check_fencing_token(
        &self,
        agent_id: AgentId,
        target: &str,
        token: i64,
    ) -> Result<bool> {
        let key = canonical_target_uri(target);
        let current: Option<i64> =
            sqlx::query("SELECT seq FROM lease_fencing_seq WHERE resource_key = ?")
                .bind(&key)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?
                .map(|r| r.try_get("seq"))
                .transpose()
                .map_err(|e| CoreError::storage(e.to_string()))?;
        if current != Some(token) {
            return Ok(false);
        }
        let live = sqlx::query(
            "SELECT 1 FROM work_leases \
             WHERE agent_id = ? AND target_uri = ? AND fencing_token = ? AND expires_at >= ?",
        )
        .bind(agent_id.to_string())
        .bind(&key)
        .bind(token)
        .bind(Utc::now().to_rfc3339())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(live.is_some())
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
        let rows = match &project_id {
            Some(p) => {
                sqlx::query(
                    "SELECT id, agent_id, task_id, project_id, path_glob, target_uri, mode, \
                 fencing_token, acquired_at, expires_at \
                 FROM work_leases WHERE expires_at >= ? AND project_id = ? \
                 ORDER BY acquired_at",
                )
                .bind(&now)
                .bind(p.to_string())
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query(
                    "SELECT id, agent_id, task_id, project_id, path_glob, target_uri, mode, \
                 fencing_token, acquired_at, expires_at \
                 FROM work_leases WHERE expires_at >= ? ORDER BY acquired_at",
                )
                .bind(&now)
                .fetch_all(&self.pool)
                .await
            }
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

    let target_uri: Option<String> = r
        .try_get("target_uri")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let mode_s: String = r
        .try_get("mode")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let fencing_token: Option<i64> = r
        .try_get("fencing_token")
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
        target_uri,
        mode: LeaseMode::parse(&mode_s).unwrap_or_default(),
        fencing_token,
        acquired_at: parse_ts(&acquired_at)?,
        expires_at: parse_ts(&expires_at)?,
    })
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
    async fn exclusive_artifact_target_one_winner() {
        let (_db, repo) = make_repo().await;
        let proj = ProjectId::new();
        let (a1, a2) = (AgentId::new(), AgentId::new());

        let r1 = repo
            .try_reserve_targets(
                a1,
                TaskId::new(),
                Some(proj),
                vec!["artifact://api/users".into()],
                LeaseMode::Exclusive,
                Duration::seconds(60),
            )
            .await
            .unwrap();
        let ReserveOutcome::Reserved { leases } = r1 else {
            panic!("first reserve must win");
        };
        assert_eq!(leases[0].mode, LeaseMode::Exclusive);
        assert_eq!(
            leases[0].target_uri.as_deref(),
            Some("artifact://api/users")
        );
        assert_eq!(leases[0].fencing_token, Some(1));

        let r2 = repo
            .try_reserve_targets(
                a2,
                TaskId::new(),
                Some(proj),
                vec!["artifact://api/users/".into()], // canonicalizes onto the same URI
                LeaseMode::Exclusive,
                Duration::seconds(60),
            )
            .await
            .unwrap();
        match r2 {
            ReserveOutcome::Conflict { holder, .. } => assert_eq!(holder, a1),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shared_read_leases_coexist_but_block_writers() {
        let (_db, repo) = make_repo().await;
        let proj = ProjectId::new();
        let (a1, a2, a3) = (AgentId::new(), AgentId::new(), AgentId::new());
        let target = vec!["contract://api/dashboard@v1".to_string()];

        for agent in [a1, a2] {
            let out = repo
                .try_reserve_targets(
                    agent,
                    TaskId::new(),
                    Some(proj),
                    target.clone(),
                    LeaseMode::SharedRead,
                    Duration::seconds(60),
                )
                .await
                .unwrap();
            assert!(
                matches!(out, ReserveOutcome::Reserved { .. }),
                "shared_read leases must coexist"
            );
        }

        let w = repo
            .try_reserve_targets(
                a3,
                TaskId::new(),
                Some(proj),
                target,
                LeaseMode::Exclusive,
                Duration::seconds(60),
            )
            .await
            .unwrap();
        assert!(
            matches!(w, ReserveOutcome::Conflict { .. }),
            "a writer must not preempt live readers"
        );
    }

    #[tokio::test]
    async fn review_blocks_writer_and_intent_never_blocks() {
        let (_db, repo) = make_repo().await;
        let proj = ProjectId::new();
        let target = vec!["artifact://db/schema".to_string()];

        repo.try_reserve_targets(
            AgentId::new(),
            TaskId::new(),
            Some(proj),
            target.clone(),
            LeaseMode::Review,
            Duration::seconds(60),
        )
        .await
        .unwrap();

        let writer = repo
            .try_reserve_targets(
                AgentId::new(),
                TaskId::new(),
                Some(proj),
                target.clone(),
                LeaseMode::Exclusive,
                Duration::seconds(60),
            )
            .await
            .unwrap();
        assert!(matches!(writer, ReserveOutcome::Conflict { .. }));

        // Intent is advisory: it neither blocks nor is blocked.
        let intent = repo
            .try_reserve_targets(
                AgentId::new(),
                TaskId::new(),
                Some(proj),
                target,
                LeaseMode::Intent,
                Duration::seconds(60),
            )
            .await
            .unwrap();
        assert!(matches!(intent, ReserveOutcome::Reserved { .. }));
    }

    #[tokio::test]
    async fn stale_fencing_token_is_rejected_after_regrant() {
        let (_db, repo) = make_repo().await;
        let proj = ProjectId::new();
        let (a1, a2) = (AgentId::new(), AgentId::new());
        let (t1, t2) = (TaskId::new(), TaskId::new());
        let uri = "artifact://api/users";

        let r1 = repo
            .try_reserve_targets(
                a1,
                t1,
                Some(proj),
                vec![uri.into()],
                LeaseMode::Exclusive,
                Duration::seconds(60),
            )
            .await
            .unwrap();
        let ReserveOutcome::Reserved { leases } = r1 else {
            panic!()
        };
        let token1 = leases[0].fencing_token.unwrap();
        assert!(repo.check_fencing_token(a1, uri, token1).await.unwrap());

        // Holder releases (crash/TTL path is equivalent); a2 re-acquires.
        repo.release_for_task(a1, t1).await.unwrap();
        let r2 = repo
            .try_reserve_targets(
                a2,
                t2,
                Some(proj),
                vec![uri.into()],
                LeaseMode::Exclusive,
                Duration::seconds(60),
            )
            .await
            .unwrap();
        let ReserveOutcome::Reserved { leases } = r2 else {
            panic!()
        };
        let token2 = leases[0].fencing_token.unwrap();
        assert!(token2 > token1, "fencing sequence must be monotonic");

        // The stale holder's write is rejected; the live holder's passes.
        assert!(!repo.check_fencing_token(a1, uri, token1).await.unwrap());
        assert!(repo.check_fencing_token(a2, uri, token2).await.unwrap());
        // Even a2 presenting the old token is rejected.
        assert!(!repo.check_fencing_token(a2, uri, token1).await.unwrap());
    }

    #[tokio::test]
    async fn legacy_path_reserve_keeps_exclusive_semantics() {
        let (_db, repo) = make_repo().await;
        let proj = ProjectId::new();
        let out = repo
            .try_reserve(
                AgentId::new(),
                TaskId::new(),
                Some(proj),
                vec!["crates/storage/src".into()],
                Duration::seconds(60),
            )
            .await
            .unwrap();
        let ReserveOutcome::Reserved { leases } = out else {
            panic!()
        };
        assert_eq!(leases[0].mode, LeaseMode::Exclusive);
        assert_eq!(leases[0].path_glob, "crates/storage/src");
        assert_eq!(
            leases[0].target_uri.as_deref(),
            Some("file://crates/storage/src")
        );
        assert!(leases[0].fencing_token.is_some());
    }

    /// P2 deadlock-safety: bulk acquisition is all-or-none and canonically
    /// ordered (requests are sorted before the scan; BEGIN IMMEDIATE
    /// serializes writers), so two agents grabbing the same pair of
    /// resources in *opposite* orders can never deadlock — one wins both,
    /// the other gets a clean Conflict and zero partial grants.
    #[tokio::test]
    async fn opposite_order_bulk_acquire_is_all_or_none_without_deadlock() {
        let (_db, repo) = make_repo().await;
        let repo = std::sync::Arc::new(repo);
        let proj = ProjectId::new();
        let (a1, a2) = (AgentId::new(), AgentId::new());

        let r1 = {
            let repo = repo.clone();
            tokio::spawn(async move {
                repo.try_reserve_targets(
                    a1,
                    TaskId::new(),
                    Some(proj),
                    vec!["artifact://api/users".into(), "artifact://db/schema".into()],
                    LeaseMode::Exclusive,
                    Duration::seconds(60),
                )
                .await
                .unwrap()
            })
        };
        let r2 = {
            let repo = repo.clone();
            tokio::spawn(async move {
                repo.try_reserve_targets(
                    a2,
                    TaskId::new(),
                    Some(proj),
                    vec!["artifact://db/schema".into(), "artifact://api/users".into()],
                    LeaseMode::Exclusive,
                    Duration::seconds(60),
                )
                .await
                .unwrap()
            })
        };
        // Must complete (no deadlock) within the test timeout.
        let (r1, r2) = (r1.await.unwrap(), r2.await.unwrap());

        let reserved_count = |o: &ReserveOutcome| match o {
            ReserveOutcome::Reserved { leases } => leases.len(),
            ReserveOutcome::Conflict { .. } => 0,
        };
        let (w1, w2) = (reserved_count(&r1), reserved_count(&r2));
        assert!(
            (w1 == 2 && w2 == 0) || (w1 == 0 && w2 == 2),
            "exactly one agent wins both targets, the other gets none: {w1}/{w2}"
        );
        // No partial grants linger for the loser.
        assert_eq!(repo.list_active(Some(proj)).await.unwrap().len(), 2);
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
