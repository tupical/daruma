//! AgentClaim repository — optimistic task locking with TTL.

use chrono::{DateTime, Duration, Utc};
use sqlx::{Row, SqlitePool};
use taskagent_shared::{AgentId, CoreError, Result, TaskId, Timestamp};

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

    // ── mutations ────────────────────────────────────────────────────────────

    /// Insert or replace a claim with a pre-computed `expires_at` (used by
    /// `apply_event` when replaying `AgentClaimed` events).
    pub async fn acquire_until(
        &self,
        agent_id: AgentId,
        task_id: TaskId,
        expires_at: Timestamp,
    ) -> Result<()> {
        let now = Utc::now();
        sqlx::query(
            "INSERT OR REPLACE INTO agent_claims \
             (agent_id, task_id, acquired_at, expires_at) VALUES (?, ?, ?, ?)",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .bind(now.to_rfc3339())
        .bind(expires_at.to_rfc3339())
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
    ) -> Result<Timestamp> {
        let now = Utc::now();
        let expires_at = now + ttl;

        sqlx::query(
            "INSERT OR REPLACE INTO agent_claims \
             (agent_id, task_id, acquired_at, expires_at) VALUES (?, ?, ?, ?)",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .bind(now.to_rfc3339())
        .bind(expires_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(expires_at)
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

fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| CoreError::serde(e.to_string()))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use taskagent_shared::{AgentId, TaskId};

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

        let expires_at = repo
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
            "INSERT INTO agent_claims (agent_id, task_id, acquired_at, expires_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .bind("2000-01-01T00:00:00+00:00") // definitely expired
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
}
