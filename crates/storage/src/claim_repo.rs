//! AgentClaim repository — optimistic task locking with TTL.

use crate::{event_store::append_on, parse_ts};
use chrono::{Duration, Utc};
use daruma_domain::Actor;
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{AgentId, ClaimId, CoreError, ProjectId, Result, RunId, TaskId, Timestamp};
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use std::future::Future;

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
#[derive(Debug, Clone)]
pub enum ClaimOutcome {
    /// The claim was acquired (or refreshed by the same agent).
    Acquired {
        expires_at: Timestamp,
        claim_id: ClaimId,
        event: EventEnvelope,
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
        actor: Actor,
        agent_id: AgentId,
        task_id: TaskId,
        ttl: Duration,
    ) -> Result<ClaimOutcome> {
        let now = Utc::now();
        let expires_at = now + ttl;
        let claim_id = ClaimId::new();
        self.try_acquire_exact(actor, agent_id, task_id, claim_id, expires_at)
            .await
    }

    /// Persist the claim generation and its audit event in one SQLite
    /// transaction. Used by command replay paths that already generated the
    /// claim id and expiry.
    pub async fn try_acquire_exact(
        &self,
        actor: Actor,
        agent_id: AgentId,
        task_id: TaskId,
        claim_id: ClaimId,
        expires_at: Timestamp,
    ) -> Result<ClaimOutcome> {
        let now_s = Utc::now().to_rfc3339();
        let expires_s = expires_at.to_rfc3339();
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *conn)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        let result = async {
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
            .execute(&mut *conn)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

            if res.rows_affected() == 1 {
                let event = append_on(
                    &mut *conn,
                    EventEnvelope::new(
                        actor,
                        Event::AgentClaimed {
                            agent_id,
                            task_id,
                            claim_id: Some(claim_id),
                            expires_at,
                        },
                    ),
                )
                .await?;
                return Ok(ClaimOutcome::Acquired {
                    expires_at,
                    claim_id,
                    event,
                });
            }

            let row = sqlx::query(
                "SELECT agent_id, expires_at FROM agent_claims \
                 WHERE task_id = ? AND expires_at >= ? ORDER BY expires_at DESC LIMIT 1",
            )
            .bind(task_id.to_string())
            .bind(&now_s)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
            match row {
                Some(row) => {
                    let holder: String = row
                        .try_get("agent_id")
                        .map_err(|e| CoreError::storage(e.to_string()))?;
                    let held_until: String = row
                        .try_get("expires_at")
                        .map_err(|e| CoreError::storage(e.to_string()))?;
                    Ok(ClaimOutcome::Busy {
                        holder: holder
                            .parse::<AgentId>()
                            .map_err(|e| CoreError::serde(e.to_string()))?,
                        expires_at: parse_ts(&held_until)?,
                    })
                }
                None => Ok(ClaimOutcome::Busy {
                    holder: agent_id,
                    expires_at,
                }),
            }
        }
        .await;

        match result {
            Ok(outcome) => {
                sqlx::query("COMMIT")
                    .execute(&mut *conn)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(outcome)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                Err(error)
            }
        }
    }

    #[cfg(test)]
    async fn acquire(
        &self,
        agent_id: AgentId,
        task_id: TaskId,
        ttl: Duration,
    ) -> Result<(Timestamp, ClaimId)> {
        match self
            .try_acquire(Actor::user(), agent_id, task_id, ttl)
            .await?
        {
            ClaimOutcome::Acquired {
                expires_at,
                claim_id,
                ..
            } => Ok((expires_at, claim_id)),
            ClaimOutcome::Busy { holder, .. } => Err(CoreError::conflict(format!(
                "task already claimed by {holder}"
            ))),
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

    /// Conditionally release one generation and append its audit event in the
    /// same transaction. With no expected generation, the current generation
    /// is read under the write lock and then used as the delete fence.
    pub async fn release_recorded(
        &self,
        actor: Actor,
        agent_id: AgentId,
        task_id: TaskId,
        expected_claim_id: Option<ClaimId>,
    ) -> Result<Option<EventEnvelope>> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *conn)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        let result = async {
            let claim_id = match expected_claim_id {
                Some(id) => id,
                None => {
                    let raw: Option<String> = sqlx::query_scalar(
                        "SELECT claim_id FROM agent_claims WHERE agent_id = ? AND task_id = ?",
                    )
                    .bind(agent_id.to_string())
                    .bind(task_id.to_string())
                    .fetch_optional(&mut *conn)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                    let Some(raw) = raw else { return Ok(None) };
                    raw.parse::<ClaimId>()
                        .map_err(|e| CoreError::serde(e.to_string()))?
                }
            };

            let deleted = sqlx::query(
                "DELETE FROM agent_claims WHERE agent_id = ? AND task_id = ? AND claim_id = ?",
            )
            .bind(agent_id.to_string())
            .bind(task_id.to_string())
            .bind(claim_id.to_string())
            .execute(&mut *conn)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
            if deleted.rows_affected() == 0 {
                return Ok(None);
            }
            append_on(
                &mut *conn,
                EventEnvelope::new(
                    actor,
                    Event::AgentReleased {
                        agent_id,
                        task_id,
                        claim_id: Some(claim_id),
                    },
                ),
            )
            .await
            .map(Some)
        }
        .await;

        match result {
            Ok(event) => {
                sqlx::query("COMMIT")
                    .execute(&mut *conn)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(event)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                Err(error)
            }
        }
    }

    /// Append a replicated claim event and update its projection in one
    /// transaction. The original envelope identity and origin metadata are
    /// preserved for device-sync deduplication.
    pub async fn append_replica_event(&self, envelope: EventEnvelope) -> Result<EventEnvelope> {
        if !matches!(
            envelope.payload,
            Event::AgentClaimed { .. } | Event::AgentReleased { .. }
        ) {
            return Err(CoreError::validation("expected a claim lifecycle event"));
        }

        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *conn)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        let result = async {
            match &envelope.payload {
                Event::AgentClaimed {
                    agent_id,
                    task_id,
                    claim_id: Some(claim_id),
                    expires_at,
                } => {
                    let now = Utc::now().to_rfc3339();
                    let expires_at = expires_at.to_rfc3339();
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
                    .bind(&now)
                    .bind(&expires_at)
                    .bind(claim_id.to_string())
                    .bind(&expires_at)
                    .bind(&now)
                    .bind(task_id.to_string())
                    .bind(&now)
                    .bind(agent_id.to_string())
                    .execute(&mut *conn)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                }
                Event::AgentClaimed { claim_id: None, .. } => {}
                Event::AgentReleased {
                    agent_id,
                    task_id,
                    claim_id,
                } => {
                    let mut query =
                        String::from("DELETE FROM agent_claims WHERE agent_id = ? AND task_id = ?");
                    if claim_id.is_some() {
                        query.push_str(" AND claim_id = ?");
                    }
                    let mut delete = sqlx::query(&query)
                        .bind(agent_id.to_string())
                        .bind(task_id.to_string());
                    if let Some(claim_id) = claim_id {
                        delete = delete.bind(claim_id.to_string());
                    }
                    delete
                        .execute(&mut *conn)
                        .await
                        .map_err(|e| CoreError::storage(e.to_string()))?;
                }
                _ => unreachable!(),
            }
            append_on(&mut *conn, envelope).await
        }
        .await;

        match result {
            Ok(event) => {
                sqlx::query("COMMIT")
                    .execute(&mut *conn)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(event)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                Err(error)
            }
        }
    }

    #[cfg(test)]
    async fn release(&self, agent_id: AgentId, task_id: TaskId) -> Result<()> {
        self.release_recorded(Actor::user(), agent_id, task_id, None)
            .await
            .map(|_| ())
    }

    async fn release_generation(
        &self,
        agent_id: AgentId,
        task_id: TaskId,
        claim_id: Option<ClaimId>,
    ) -> Result<()> {
        let mut query = String::from("DELETE FROM agent_claims WHERE agent_id = ? AND task_id = ?");
        if claim_id.is_some() {
            query.push_str(" AND claim_id = ?");
        }
        let mut delete = sqlx::query(&query)
            .bind(agent_id.to_string())
            .bind(task_id.to_string());
        if let Some(claim_id) = claim_id {
            delete = delete.bind(claim_id.to_string());
        }
        delete
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
            Event::AgentReleased {
                agent_id,
                task_id,
                claim_id,
            } => {
                self.release_generation(*agent_id, *task_id, *claim_id)
                    .await
            }
            // Auto-release every claim when the task closes.
            Event::TaskClosed { task_id, .. } => self.release_all_for_task(*task_id).await,
            _ => Ok(()),
        }
    }

    /// Delete expired generations and append truthful release events in the
    /// same write transaction.
    pub async fn sweep_expired(&self, actor: Actor) -> Result<Vec<EventEnvelope>> {
        self.sweep_expired_after(actor, || async {}).await
    }

    async fn sweep_expired_after<F, Fut>(
        &self,
        actor: Actor,
        after_select: F,
    ) -> Result<Vec<EventEnvelope>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = ()>,
    {
        let now = Utc::now().to_rfc3339();
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *conn)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        let result = async {
            let rows = sqlx::query(
                "SELECT agent_id, task_id, claim_id FROM agent_claims WHERE expires_at < ?",
            )
            .bind(&now)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
            after_select().await;

            let mut events = Vec::with_capacity(rows.len());
            for row in rows {
                let agent_id = row
                    .try_get::<String, _>("agent_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?
                    .parse::<AgentId>()
                    .map_err(|e| CoreError::serde(e.to_string()))?;
                let task_id = row
                    .try_get::<String, _>("task_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?
                    .parse::<TaskId>()
                    .map_err(|e| CoreError::serde(e.to_string()))?;
                let claim_id = row
                    .try_get::<String, _>("claim_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?
                    .parse::<ClaimId>()
                    .map_err(|e| CoreError::serde(e.to_string()))?;
                let deleted = sqlx::query(
                    "DELETE FROM agent_claims \
                     WHERE agent_id = ? AND task_id = ? AND claim_id = ? AND expires_at < ?",
                )
                .bind(agent_id.to_string())
                .bind(task_id.to_string())
                .bind(claim_id.to_string())
                .bind(&now)
                .execute(&mut *conn)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
                if deleted.rows_affected() == 1 {
                    events.push(
                        append_on(
                            &mut *conn,
                            EventEnvelope::new(
                                actor.clone(),
                                Event::AgentReleased {
                                    agent_id,
                                    task_id,
                                    claim_id: Some(claim_id),
                                },
                            ),
                        )
                        .await?,
                    );
                }
            }
            Ok(events)
        }
        .await;

        match result {
            Ok(events) => {
                sqlx::query("COMMIT")
                    .execute(&mut *conn)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(events)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                Err(error)
            }
        }
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

        let released = repo.sweep_expired(Actor::user()).await.unwrap();
        assert_eq!(released.len(), 1);
        assert!(matches!(
            released[0].payload,
            Event::AgentReleased {
                agent_id: a,
                task_id: t,
                claim_id: Some(_),
            } if a == agent_id && t == task_id
        ));

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
            .try_acquire(Actor::user(), a1, task_id, Duration::seconds(60))
            .await
            .unwrap();
        assert!(matches!(out1, ClaimOutcome::Acquired { .. }));

        // Second agent is told it's busy, and by whom.
        let out2 = repo
            .try_acquire(Actor::user(), a2, task_id, Duration::seconds(60))
            .await
            .unwrap();
        match out2 {
            ClaimOutcome::Busy { holder, .. } => assert_eq!(holder, a1),
            other => panic!("expected Busy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn acquire_audit_failure_rolls_back_claim() {
        let (db, repo) = make_repo().await;
        sqlx::query(
            "CREATE TRIGGER fail_claim_audit BEFORE INSERT ON events \
             WHEN NEW.kind = 'agent_claimed' \
             BEGIN SELECT RAISE(FAIL, 'injected audit failure'); END",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let result = repo
            .try_acquire(
                Actor::user(),
                AgentId::new(),
                TaskId::new(),
                Duration::seconds(60),
            )
            .await;
        assert!(result.is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_claims")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM events")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn replica_audit_failure_rolls_back_claim_projection() {
        let (db, repo) = make_repo().await;
        sqlx::query(
            "CREATE TRIGGER fail_replica_claim_audit BEFORE INSERT ON events \
             WHEN NEW.kind = 'agent_claimed' \
             BEGIN SELECT RAISE(FAIL, 'injected replica audit failure'); END",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let result = repo
            .append_replica_event(EventEnvelope::new(
                Actor::user(),
                Event::AgentClaimed {
                    agent_id: AgentId::new(),
                    task_id: TaskId::new(),
                    claim_id: Some(ClaimId::new()),
                    expires_at: Utc::now() + Duration::seconds(60),
                },
            ))
            .await;
        assert!(result.is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_claims")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM events")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn release_audit_failure_preserves_exact_claim() {
        let (db, repo) = make_repo().await;
        let agent_id = AgentId::new();
        let task_id = TaskId::new();
        let claim_id = match repo
            .try_acquire(Actor::user(), agent_id, task_id, Duration::seconds(60))
            .await
            .unwrap()
        {
            ClaimOutcome::Acquired { claim_id, .. } => claim_id,
            other => panic!("expected acquired, got {other:?}"),
        };
        sqlx::query(
            "CREATE TRIGGER fail_release_audit BEFORE INSERT ON events \
             WHEN NEW.kind = 'agent_released' \
             BEGIN SELECT RAISE(FAIL, 'injected release audit failure'); END",
        )
        .execute(db.pool())
        .await
        .unwrap();

        assert!(repo
            .release_recorded(Actor::user(), agent_id, task_id, Some(claim_id))
            .await
            .is_err());
        let persisted: String = sqlx::query_scalar(
            "SELECT claim_id FROM agent_claims WHERE agent_id = ? AND task_id = ?",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(persisted, claim_id.to_string());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM events WHERE kind = 'agent_released'",
            )
            .fetch_one(db.pool())
            .await
            .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn sweep_selection_cannot_delete_concurrent_refresh() {
        use std::sync::Arc;
        use tokio::sync::Barrier;

        let (db, repo) = make_repo().await;
        let repo = Arc::new(repo);
        let agent_id = AgentId::new();
        let task_id = TaskId::new();
        let expired_claim_id = ClaimId::new();
        sqlx::query(
            "INSERT INTO agent_claims (agent_id, task_id, acquired_at, expires_at, claim_id) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .bind("2000-01-01T00:00:00+00:00")
        .bind(expired_claim_id.to_string())
        .execute(db.pool())
        .await
        .unwrap();

        let selected = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let sweep = tokio::spawn({
            let repo = repo.clone();
            let selected = selected.clone();
            let resume = resume.clone();
            async move {
                repo.sweep_expired_after(Actor::user(), move || async move {
                    selected.wait().await;
                    resume.wait().await;
                })
                .await
            }
        });
        selected.wait().await;

        let mut refresh = tokio::spawn({
            let repo = repo.clone();
            async move {
                repo.try_acquire(Actor::user(), agent_id, task_id, Duration::seconds(60))
                    .await
            }
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut refresh)
                .await
                .is_err()
        );
        resume.wait().await;

        let release_events = sweep.await.unwrap().unwrap();
        let refreshed_claim_id = match refresh.await.unwrap().unwrap() {
            ClaimOutcome::Acquired { claim_id, .. } => claim_id,
            other => panic!("expected refreshed claim, got {other:?}"),
        };
        assert_ne!(refreshed_claim_id, expired_claim_id);
        assert!(matches!(
            release_events.as_slice(),
            [EventEnvelope {
                payload: Event::AgentReleased {
                    claim_id: Some(id),
                    ..
                },
                ..
            }] if *id == expired_claim_id
        ));
        assert_eq!(
            repo.list_active(None).await.unwrap()[0].claim_id,
            refreshed_claim_id
        );
    }

    #[tokio::test]
    async fn try_acquire_same_agent_refreshes() {
        let (_db, repo) = make_repo().await;
        let task_id = TaskId::new();
        let agent = AgentId::new();

        let out1 = repo
            .try_acquire(Actor::user(), agent, task_id, Duration::seconds(60))
            .await
            .unwrap();
        let first = match out1 {
            ClaimOutcome::Acquired {
                expires_at,
                claim_id,
                event,
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
                assert!(matches!(
                    event.payload,
                    Event::AgentClaimed {
                        claim_id: Some(event_claim_id),
                        ..
                    } if event_claim_id == claim_id
                ));
                (expires_at, claim_id)
            }
            other => panic!("expected Acquired, got {other:?}"),
        };

        let out2 = repo
            .try_acquire(Actor::user(), agent, task_id, Duration::seconds(600))
            .await
            .unwrap();
        match out2 {
            ClaimOutcome::Acquired {
                expires_at,
                claim_id,
                ..
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
            .try_acquire(Actor::user(), agent_id, task_id, Duration::seconds(120))
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
        repo.try_acquire(Actor::user(), second, task_id, Duration::seconds(60))
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
            .try_acquire(Actor::user(), fresh, task_id, Duration::seconds(60))
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
        repo.release(me, task_id).await.unwrap();
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

        let released = repo.sweep_expired(Actor::user()).await.unwrap();
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
