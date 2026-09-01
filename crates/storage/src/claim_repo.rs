//! AgentClaim repository — optimistic task locking with TTL.

use crate::parse_ts;
use chrono::{Duration, Utc};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{AgentId, ClaimId, CoreError, ProjectId, Result, RunId, TaskId, Timestamp};
use serde::Serialize;
use sqlx::{Row, SqlitePool};

/// A live task claim (agent → task lock) as surfaced by the Agent Operations
/// read layer. Mirrors an `agent_claims` row that has not yet expired.
#[derive(Debug, Clone, Serialize)]
pub struct ActiveClaim {
    pub agent_id: AgentId,
    pub task_id: TaskId,
    pub acquired_at: Timestamp,
    pub expires_at: Timestamp,
    pub run_id: Option<RunId>,
    pub claim_id: ClaimId,
}

/// Outcome of an atomic [`AgentClaimRepo::try_acquire`] attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The claim was acquired (or refreshed by the same agent).
    Acquired {
        expires_at: Timestamp,
        claim_id: ClaimId,
    },
    /// Another agent holds a live claim — the task is taken.
    Busy {
        holder: AgentId,
        expires_at: Timestamp,
    },
}

/// Read/write access to the `agent_claims` table.
pub struct AgentClaimRepo {
    pub(crate) pool: SqlitePool,
}

impl AgentClaimRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── queries ──────────────────────────────────────────────────────────────

    /// Check whether `task_id` is currently claimed.
    ///
    /// Returns `Some((agent_id, expires_at))` if a non-expired claim exists,
    /// `None` otherwise.
    pub async fn is_claimed(&self, task_id: TaskId) -> Result<Option<(AgentId, Timestamp)>> {
        let now = Utc::now().to_rfc3339();

        let row = sqlx::query(
            "SELECT agent_id, expires_at FROM agent_claims \
             WHERE task_id = ? AND expires_at >= ? \
             ORDER BY expires_at DESC LIMIT 1",
        )
        .bind(task_id.to_string())
        .bind(&now)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        match row {
            None => Ok(None),
            Some(r) => {
                let agent_id_s: String = r
                    .try_get("agent_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let expires_at_s: String = r
                    .try_get("expires_at")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let agent_id = agent_id_s
                    .parse::<AgentId>()
                    .map_err(|e| CoreError::serde(e.to_string()))?;
                let expires_at = parse_ts(&expires_at_s)?;
                Ok(Some((agent_id, expires_at)))
            }
        }
    }

    /// Return the agent holding a live claim on `task_id` that is **not**
    /// `agent_id`, if any. Used by the claim-aware next-task resolver to skip
    /// tasks already taken by a different agent.
    pub async fn is_claimed_by_other(
        &self,
        task_id: TaskId,
        agent_id: AgentId,
    ) -> Result<Option<AgentId>> {
        let now = Utc::now().to_rfc3339();
        let row = sqlx::query(
            "SELECT agent_id FROM agent_claims \
             WHERE task_id = ? AND expires_at >= ? AND agent_id <> ? \
             ORDER BY expires_at DESC LIMIT 1",
        )
        .bind(task_id.to_string())
        .bind(&now)
        .bind(agent_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        match row {
            None => Ok(None),
            Some(r) => {
                let s: String = r
                    .try_get("agent_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(Some(
                    s.parse::<AgentId>()
                        .map_err(|e| CoreError::serde(e.to_string()))?,
                ))
            }
        }
    }

    /// Return all agent IDs that hold an active (non-expired) claim on `task_id`.
    pub async fn get_agents_claiming_task(&self, task_id: TaskId) -> Result<Vec<AgentId>> {
        let now = Utc::now().to_rfc3339();
        let rows =
            sqlx::query("SELECT agent_id FROM agent_claims WHERE task_id = ? AND expires_at >= ?")
                .bind(task_id.to_string())
                .bind(&now)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter()
            .map(|r| {
                let s: String = r
                    .try_get("agent_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                s.parse::<AgentId>()
                    .map_err(|e| CoreError::serde(e.to_string()))
            })
            .collect()
    }

    /// List all live (non-expired) claims, optionally scoped to a project.
    ///
    /// "Active" mirrors [`sweep_expired`](Self::sweep_expired)/`is_claimed`:
    /// a row exists **and** `expires_at >= now` (released claims are hard
    /// `DELETE`d, expired ones are swept). `agent_claims` has no `project_id`
    /// column, so scope is applied via an `EXISTS` against `tasks`.
    pub async fn list_active(&self, project_id: Option<ProjectId>) -> Result<Vec<ActiveClaim>> {
        let now = Utc::now().to_rfc3339();
        let rows = match &project_id {
            Some(p) => {
                sqlx::query(
                    "SELECT agent_id, task_id, acquired_at, expires_at, run_id, claim_id FROM agent_claims \
                     WHERE expires_at >= ? AND EXISTS ( \
                         SELECT 1 FROM tasks \
                         WHERE tasks.id = agent_claims.task_id AND tasks.project_id = ?) \
                     ORDER BY acquired_at",
                )
                .bind(&now)
                .bind(p.to_string())
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query(
                    "SELECT agent_id, task_id, acquired_at, expires_at, run_id, claim_id FROM agent_claims \
                     WHERE expires_at >= ? ORDER BY acquired_at",
                )
                .bind(&now)
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_active_claim).collect()
    }

    // ── mutations ────────────────────────────────────────────────────────────

    /// Insert or replace a claim with a pre-computed `expires_at` (used by
    /// `apply_event` when replaying `AgentClaimed` events).
    pub async fn acquire_until(
        &self,
        agent_id: AgentId,
        task_id: TaskId,
        claim_id: ClaimId,
        expires_at: Timestamp,
    ) -> Result<()> {
        let now = Utc::now();
        let now_s = now.to_rfc3339();
        let expires_s = expires_at.to_rfc3339();
        sqlx::query(
            "INSERT INTO agent_claims \
             (agent_id, task_id, acquired_at, expires_at, claim_id) \
             SELECT ?, ?, ?, ?, ? \
             WHERE ? > ? AND NOT EXISTS ( \
                 SELECT 1 FROM agent_claims \
                 WHERE task_id = ? AND expires_at >= ? AND agent_id <> ? \
             ) \
             ON CONFLICT(agent_id, task_id) DO UPDATE SET \
                 acquired_at = excluded.acquired_at, expires_at = excluded.expires_at \
             WHERE agent_claims.claim_id = excluded.claim_id",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .bind(&now_s)
        .bind(&expires_s)
        .bind(claim_id.to_string())
        .bind(&expires_s)
        .bind(&now_s)
        .bind(task_id.to_string())
        .bind(&now_s)
        .bind(agent_id.to_string())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Acquire (or refresh) a claim on `task_id` for `ttl` duration.
    ///
    /// Uses `INSERT OR REPLACE` so re-acquiring extends the TTL.
    /// Returns the computed `expires_at` timestamp.
    pub async fn acquire(
        &self,
        agent_id: AgentId,
        task_id: TaskId,
        ttl: Duration,
    ) -> Result<(Timestamp, ClaimId)> {
        let now = Utc::now();
        let expires_at = now + ttl;
        let claim_id = ClaimId::new();

        sqlx::query(
            "INSERT OR REPLACE INTO agent_claims \
             (agent_id, task_id, acquired_at, expires_at, claim_id) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .bind(now.to_rfc3339())
        .bind(expires_at.to_rfc3339())
        .bind(claim_id.to_string())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok((expires_at, claim_id))
    }

    /// Atomically acquire an **exclusive** claim on `task_id` for `ttl`.
    ///
    /// Exclusivity is enforced by SQLite at the statement level: the row is
    /// inserted only when no *other* agent holds a live (non-expired) claim.
    /// The same agent re-acquiring simply refreshes its TTL (upsert). This is
    /// the compare-and-set primitive the concurrent `drain_next` / `claim`
    /// paths rely on — the generic [`acquire`](Self::acquire) is non-atomic and
    /// kept only for event replay.
    ///
    /// Returns [`ClaimOutcome::Acquired`] on success, or [`ClaimOutcome::Busy`]
    /// with the current holder when another agent owns the task.
    pub async fn try_acquire(
        &self,
        agent_id: AgentId,
        task_id: TaskId,
        ttl: Duration,
    ) -> Result<ClaimOutcome> {
        let now = Utc::now();
        let now_s = now.to_rfc3339();
        let expires_at = now + ttl;
        let expires_s = expires_at.to_rfc3339();
        let claim_id = ClaimId::new();

        // Single-statement CAS: insert iff no *other* agent holds a live claim;
        // on PK conflict (same agent re-acquiring) refresh the TTL. A lone
        // INSERT statement runs under SQLite's write lock, so two concurrent
        // callers serialize and the loser inserts zero rows.
        let res = sqlx::query(
            "INSERT INTO agent_claims (agent_id, task_id, acquired_at, expires_at, claim_id) \
             SELECT ?, ?, ?, ?, ? \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM agent_claims \
                 WHERE task_id = ? AND expires_at >= ? AND agent_id <> ? \
             ) \
             ON CONFLICT(agent_id, task_id) DO UPDATE SET \
                 acquired_at = excluded.acquired_at, \
                 expires_at  = excluded.expires_at, \
                 claim_id    = excluded.claim_id",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .bind(&now_s)
        .bind(&expires_s)
        .bind(claim_id.to_string())
        .bind(task_id.to_string())
        .bind(&now_s)
        .bind(agent_id.to_string())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        if res.rows_affected() >= 1 {
            return Ok(ClaimOutcome::Acquired {
                expires_at,
                claim_id,
            });
        }

        // Insert was suppressed → another agent holds it. Report the holder.
        match self.is_claimed(task_id).await? {
            Some((holder, exp)) => Ok(ClaimOutcome::Busy {
                holder,
                expires_at: exp,
            }),
            // Claim vanished between the CAS and this read (expired/released).
            // Treat as a transient loss; the caller retries against the pool.
            None => Ok(ClaimOutcome::Busy {
                holder: agent_id,
                expires_at,
            }),
        }
    }

    /// Release **all** claims on a task, regardless of holder. Used to
    /// auto-clean claims when a task closes.
    pub async fn release_all_for_task(&self, task_id: TaskId) -> Result<()> {
        sqlx::query("DELETE FROM agent_claims WHERE task_id = ?")
            .bind(task_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Release a specific agent's claim on a task.
    pub async fn release(&self, agent_id: AgentId, task_id: TaskId) -> Result<()> {
        sqlx::query("DELETE FROM agent_claims WHERE agent_id = ? AND task_id = ?")
            .bind(agent_id.to_string())
            .bind(task_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Apply a persisted event to the projection.
    pub async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        match &env.payload {
            Event::AgentClaimed {
                agent_id,
                task_id,
                claim_id: Some(claim_id),
                expires_at,
            } => {
                self.acquire_until(*agent_id, *task_id, *claim_id, *expires_at)
                    .await
            }
            Event::AgentClaimed { claim_id: None, .. } => Ok(()),
            Event::AgentReleased { agent_id, task_id } => self.release(*agent_id, *task_id).await,
            // Auto-release every claim when the task closes.
            Event::TaskClosed { task_id, .. } => self.release_all_for_task(*task_id).await,
            _ => Ok(()),
        }
    }

    /// Delete all expired claims and return the `(agent_id, task_id)` pairs
    /// that were released so callers can emit `AgentReleased` events.
    pub async fn sweep_expired(&self) -> Result<Vec<(AgentId, TaskId)>> {
        let now = Utc::now().to_rfc3339();

        // Collect before deleting.
        let rows = sqlx::query("SELECT agent_id, task_id FROM agent_claims WHERE expires_at < ?")
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
            sqlx::query("DELETE FROM agent_claims WHERE expires_at < ?")
                .bind(&now)
                .execute(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
        }

        Ok(pairs)
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn row_to_active_claim(r: &sqlx::sqlite::SqliteRow) -> Result<ActiveClaim> {
    let agent_id: String = r
        .try_get("agent_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let task_id: String = r
        .try_get("task_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let acquired_at: String = r
        .try_get("acquired_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let expires_at: String = r
        .try_get("expires_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let run_id: Option<String> = r
        .try_get("run_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let claim_id: String = r
        .try_get("claim_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    Ok(ActiveClaim {
        agent_id: agent_id
            .parse::<AgentId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        task_id: task_id
            .parse::<TaskId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        acquired_at: parse_ts(&acquired_at)?,
        expires_at: parse_ts(&expires_at)?,
        run_id: run_id
            .map(|id| {
                id.parse::<RunId>()
                    .map_err(|e| CoreError::serde(e.to_string()))
            })
            .transpose()?,
        claim_id: claim_id
            .parse::<ClaimId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_shared::{AgentId, TaskId};

    async fn make_repo() -> (Db, AgentClaimRepo) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = AgentClaimRepo::new(db.pool().clone());
        (db, repo)
    }

    #[tokio::test]
    async fn claim_acquire_and_check() {
        let (_db, repo) = make_repo().await;
        let agent_id = AgentId::new();
        let task_id = TaskId::new();

        let (expires_at, _) = repo
            .acquire(agent_id, task_id, Duration::seconds(60))
            .await
            .unwrap();

        assert!(expires_at > Utc::now());

        let claimed = repo.is_claimed(task_id).await.unwrap();
        assert!(claimed.is_some());
        let (claimant, _) = claimed.unwrap();
        assert_eq!(claimant, agent_id);
    }

    #[tokio::test]
    async fn claim_release_removes_claim() {
        let (_db, repo) = make_repo().await;
        let agent_id = AgentId::new();
        let task_id = TaskId::new();

        repo.acquire(agent_id, task_id, Duration::seconds(60))
            .await
            .unwrap();
        assert!(repo.is_claimed(task_id).await.unwrap().is_some());

        repo.release(agent_id, task_id).await.unwrap();
        assert!(repo.is_claimed(task_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn claim_unclaimed_task_returns_none() {
        let (_db, repo) = make_repo().await;
        let result = repo.is_claimed(TaskId::new()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn claim_sweep_expired_returns_released_pairs() {
        let (db, repo) = make_repo().await;
        let agent_id = AgentId::new();
        let task_id = TaskId::new();

        // Insert a claim with expires_at in the past.
        sqlx::query(
            "INSERT INTO agent_claims (agent_id, task_id, acquired_at, expires_at, claim_id) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .bind("2000-01-01T00:00:00+00:00") // definitely expired
        .bind(ClaimId::new().to_string())
        .execute(db.pool())
        .await
        .unwrap();

        let released = repo.sweep_expired().await.unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].0, agent_id);
        assert_eq!(released[0].1, task_id);

        // Verify the row is gone.
        let still_claimed = repo.is_claimed(task_id).await.unwrap();
        assert!(still_claimed.is_none());
    }

    #[tokio::test]
    async fn try_acquire_is_exclusive_across_agents() {
        let (_db, repo) = make_repo().await;
        let task_id = TaskId::new();
        let a1 = AgentId::new();
        let a2 = AgentId::new();

        // First agent wins.
        let out1 = repo
            .try_acquire(a1, task_id, Duration::seconds(60))
            .await
            .unwrap();
        assert!(matches!(out1, ClaimOutcome::Acquired { .. }));

        // Second agent is told it's busy, and by whom.
        let out2 = repo
            .try_acquire(a2, task_id, Duration::seconds(60))
            .await
            .unwrap();
        match out2 {
            ClaimOutcome::Busy { holder, .. } => assert_eq!(holder, a1),
            other => panic!("expected Busy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn try_acquire_same_agent_refreshes() {
        let (_db, repo) = make_repo().await;
        let task_id = TaskId::new();
        let agent = AgentId::new();

        let out1 = repo
            .try_acquire(agent, task_id, Duration::seconds(60))
            .await
            .unwrap();
        let first = match out1 {
            ClaimOutcome::Acquired {
                expires_at,
                claim_id,
            } => {
                let persisted: String = sqlx::query_scalar(
                    "SELECT claim_id FROM agent_claims WHERE agent_id = ? AND task_id = ?",
                )
                .bind(agent.to_string())
                .bind(task_id.to_string())
                .fetch_one(&repo.pool)
                .await
                .unwrap();
                assert_eq!(persisted, claim_id.to_string());
                (expires_at, claim_id)
            }
            other => panic!("expected Acquired, got {other:?}"),
        };

        let out2 = repo
            .try_acquire(agent, task_id, Duration::seconds(600))
            .await
            .unwrap();
        match out2 {
            ClaimOutcome::Acquired {
                expires_at,
                claim_id,
            } => {
                assert!(expires_at >= first.0);
                assert_ne!(claim_id, first.1);
                let persisted: String = sqlx::query_scalar(
                    "SELECT claim_id FROM agent_claims WHERE agent_id = ? AND task_id = ?",
                )
                .bind(agent.to_string())
                .bind(task_id.to_string())
                .fetch_one(&repo.pool)
                .await
                .unwrap();
                assert_eq!(persisted, claim_id.to_string());
            }
            other => panic!("expected refreshed Acquired, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stale_claim_event_cannot_replace_a_newer_generation() {
        let (db, repo) = make_repo().await;
        let agent_id = AgentId::new();
        let task_id = TaskId::new();
        let stale_id = ClaimId::new();
        let stale_expiry = Utc::now() + Duration::seconds(60);

        repo.acquire_until(agent_id, task_id, stale_id, stale_expiry)
            .await
            .unwrap();
        let current_id = match repo
            .try_acquire(agent_id, task_id, Duration::seconds(120))
            .await
            .unwrap()
        {
            ClaimOutcome::Acquired { claim_id, .. } => claim_id,
            other => panic!("expected acquired, got {other:?}"),
        };

        repo.acquire_until(agent_id, task_id, stale_id, stale_expiry)
            .await
            .unwrap();
        let persisted: String = sqlx::query_scalar(
            "SELECT claim_id FROM agent_claims WHERE agent_id = ? AND task_id = ?",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(persisted, current_id.to_string());
    }

    #[tokio::test]
    async fn delayed_claim_event_cannot_restore_a_previous_holder() {
        let (_db, repo) = make_repo().await;
        let first = AgentId::new();
        let second = AgentId::new();
        let task_id = TaskId::new();
        let stale_id = ClaimId::new();

        repo.acquire_until(first, task_id, stale_id, Utc::now() - Duration::seconds(1))
            .await
            .unwrap();
        repo.try_acquire(second, task_id, Duration::seconds(60))
            .await
            .unwrap();
        repo.acquire_until(first, task_id, stale_id, Utc::now() + Duration::seconds(60))
            .await
            .unwrap();

        assert_eq!(
            repo.get_agents_claiming_task(task_id).await.unwrap(),
            vec![second]
        );
    }

    #[tokio::test]
    async fn try_acquire_takes_over_expired_claim() {
        let (db, repo) = make_repo().await;
        let task_id = TaskId::new();
        let stale = AgentId::new();
        let fresh = AgentId::new();

        // Insert an expired claim held by `stale`.
        sqlx::query(
            "INSERT INTO agent_claims (agent_id, task_id, acquired_at, expires_at, claim_id) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(stale.to_string())
        .bind(task_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .bind("2000-01-01T00:00:00+00:00")
        .bind(ClaimId::new().to_string())
        .execute(db.pool())
        .await
        .unwrap();

        // A different agent may still acquire because the prior claim is expired.
        let out = repo
            .try_acquire(fresh, task_id, Duration::seconds(60))
            .await
            .unwrap();
        assert!(matches!(out, ClaimOutcome::Acquired { .. }));
        let (holder, _) = repo.is_claimed(task_id).await.unwrap().unwrap();
        assert_eq!(holder, fresh);
    }

    #[tokio::test]
    async fn is_claimed_by_other_ignores_self_and_expired() {
        let (_db, repo) = make_repo().await;
        let task_id = TaskId::new();
        let me = AgentId::new();
        let them = AgentId::new();

        // My own live claim must not count as "claimed by other".
        repo.acquire(me, task_id, Duration::seconds(60))
            .await
            .unwrap();
        assert_eq!(repo.is_claimed_by_other(task_id, me).await.unwrap(), None);

        // Another agent's live claim does.
        repo.acquire(them, task_id, Duration::seconds(60))
            .await
            .unwrap();
        assert_eq!(
            repo.is_claimed_by_other(task_id, me).await.unwrap(),
            Some(them)
        );
    }

    #[tokio::test]
    async fn list_active_returns_live_claims_and_scopes_by_project() {
        use daruma_shared::ProjectId;
        let (db, repo) = make_repo().await;
        let project = ProjectId::new();
        let agent = AgentId::new();
        let task = TaskId::new();

        // Seed a task in `project` so the EXISTS scope can match it.
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO tasks (id, project_id, title, created_at, updated_at) \
             VALUES (?, ?, 'parent', ?, ?)",
        )
        .bind(task.to_string())
        .bind(project.to_string())
        .bind(&now)
        .bind(&now)
        .execute(db.pool())
        .await
        .unwrap();

        repo.acquire(agent, task, Duration::seconds(60))
            .await
            .unwrap();

        // Unscoped sees it.
        let all = repo.list_active(None).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].agent_id, agent);
        assert_eq!(all[0].task_id, task);

        // Scoped to the right project sees it; a foreign project does not.
        assert_eq!(repo.list_active(Some(project)).await.unwrap().len(), 1);
        assert_eq!(
            repo.list_active(Some(ProjectId::new()))
                .await
                .unwrap()
                .len(),
            0
        );

        // Released → gone (hard DELETE).
        repo.release(agent, task).await.unwrap();
        assert!(repo.list_active(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_active_excludes_expired() {
        let (db, repo) = make_repo().await;
        let agent = AgentId::new();
        let task = TaskId::new();
        sqlx::query(
            "INSERT INTO agent_claims (agent_id, task_id, acquired_at, expires_at, claim_id) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(agent.to_string())
        .bind(task.to_string())
        .bind(Utc::now().to_rfc3339())
        .bind("2000-01-01T00:00:00+00:00")
        .bind(ClaimId::new().to_string())
        .execute(db.pool())
        .await
        .unwrap();

        assert!(repo.list_active(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn claim_sweep_does_not_remove_active_claims() {
        let (_db, repo) = make_repo().await;
        let agent_id = AgentId::new();
        let task_id = TaskId::new();

        repo.acquire(agent_id, task_id, Duration::seconds(300))
            .await
            .unwrap();

        let released = repo.sweep_expired().await.unwrap();
        assert!(released.is_empty());

        // Still claimed.
        assert!(repo.is_claimed(task_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn migration_0053_preserves_live_claims_and_backfills_unique_generations() {
        let db = Db::memory().await.unwrap();
        let pool = db.pool();
        sqlx::query(
            "CREATE TABLE agent_claims (\
                 agent_id TEXT NOT NULL, task_id TEXT NOT NULL, \
                 acquired_at TEXT NOT NULL, expires_at TEXT NOT NULL, \
                 PRIMARY KEY (agent_id, task_id))",
        )
        .execute(pool)
        .await
        .unwrap();

        let legacy = [
            (
                AgentId::new().to_string(),
                TaskId::new().to_string(),
                "2026-08-31T01:00:00Z",
                "2026-09-01T04:00:00Z",
            ),
            (
                AgentId::new().to_string(),
                TaskId::new().to_string(),
                "2026-08-31T02:00:00Z",
                "2026-09-01T05:00:00Z",
            ),
        ];
        for (agent_id, task_id, acquired_at, expires_at) in &legacy {
            sqlx::query(
                "INSERT INTO agent_claims (agent_id, task_id, acquired_at, expires_at) \
                 VALUES (?, ?, ?, ?)",
            )
            .bind(agent_id)
            .bind(task_id)
            .bind(acquired_at)
            .bind(expires_at)
            .execute(pool)
            .await
            .unwrap();
        }

        sqlx::raw_sql(include_str!("../migrations/0053_claim_run_ownership.sql"))
            .execute(pool)
            .await
            .unwrap();

        let rows: Vec<(String, String, String, String, Option<String>, String)> = sqlx::query_as(
            "SELECT agent_id, task_id, acquired_at, expires_at, run_id, claim_id \
                 FROM agent_claims ORDER BY acquired_at",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), legacy.len());
        for (row, expected) in rows.iter().zip(legacy.iter()) {
            assert_eq!(row.0, expected.0);
            assert_eq!(row.1, expected.1);
            assert_eq!(row.2, expected.2);
            assert_eq!(row.3, expected.3);
            assert!(row.4.is_none(), "pre-0053 claims have no safe run binding");
            uuid::Uuid::parse_str(
                row.5
                    .strip_prefix("clm_")
                    .expect("claim generation uses the typed clm_ UUID form"),
            )
            .expect("claim generation must contain a UUID");
        }
        assert_ne!(rows[0].5, rows[1].5);
    }
}
