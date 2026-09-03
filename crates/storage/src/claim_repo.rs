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

/// Outcome of a claim CAS whose mutation and audit event share one transaction.
#[derive(Debug, Clone)]
pub enum RecordedClaimOutcome {
    Acquired {
        expires_at: Timestamp,
        claim_id: ClaimId,
        event: EventEnvelope,
    },
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
            "INSERT INTO agent_claims \
                (agent_id, task_id, acquired_at, expires_at, run_id, claim_id) \
             SELECT ?, ?, ?, ?, NULL, ? \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM agent_claims \
                 WHERE task_id = ? AND expires_at >= ? AND agent_id <> ? \
             ) \
             ON CONFLICT(agent_id, task_id) DO UPDATE SET \
                 acquired_at = excluded.acquired_at, \
                 expires_at  = excluded.expires_at, \
                 run_id      = CASE \
                     WHEN agent_claims.expires_at >= ? THEN agent_claims.run_id \
                     ELSE NULL \
                 END, \
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
        .bind(&now_s)
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

    /// Acquire or refresh a claim and persist its generation-bearing audit
    /// event in the same SQLite transaction.
    ///
    /// A runless refresh keeps an existing `run_id` only while that row is
    /// still live. Reacquiring an expired generation starts a new runless
    /// generation with `run_id = NULL`, so stale run ownership never leaks
    /// across expiry.
    pub async fn try_acquire_recorded(
        &self,
        actor: Actor,
        agent_id: AgentId,
        task_id: TaskId,
        ttl: Duration,
    ) -> Result<RecordedClaimOutcome> {
        let now = Utc::now();
        self.try_acquire_exact_recorded(actor, agent_id, task_id, ClaimId::new(), now + ttl)
            .await
    }

    /// Recorded acquire with caller-provided generation and expiry, used by
    /// the command seam after it has built the exact claim contract.
    pub async fn try_acquire_exact_recorded(
        &self,
        actor: Actor,
        agent_id: AgentId,
        task_id: TaskId,
        claim_id: ClaimId,
        expires_at: Timestamp,
    ) -> Result<RecordedClaimOutcome> {
        self.try_acquire_bound_recorded(actor, agent_id, task_id, claim_id, expires_at, None)
            .await
    }

    /// Acquire a claim for an authenticated active run. Run existence, plan,
    /// active state, owner and claim CAS are checked by the same transaction.
    pub async fn try_acquire_for_run_recorded(
        &self,
        actor: Actor,
        owner_agent_id: AgentId,
        run_id: RunId,
        plan_id: daruma_shared::PlanId,
        task_id: TaskId,
        claim_id: ClaimId,
        expires_at: Timestamp,
    ) -> Result<RecordedClaimOutcome> {
        self.try_acquire_bound_recorded(
            actor,
            owner_agent_id,
            task_id,
            claim_id,
            expires_at,
            Some((run_id, plan_id)),
        )
        .await
    }

    /// Fail before readiness resolution when a supplied run cannot authorize
    /// this plan drain. Claim acquisition repeats the check transactionally.
    pub async fn validate_run_assignment(
        &self,
        owner_agent_id: AgentId,
        run_id: RunId,
        plan_id: daruma_shared::PlanId,
    ) -> Result<()> {
        let actual_plan_id = self.resolve_run_assignment(owner_agent_id, run_id).await?;
        if actual_plan_id != plan_id {
            return Err(CoreError::conflict("run belongs to a different plan"));
        }
        Ok(())
    }

    /// Resolve an authenticated active run to its active plan.
    pub async fn resolve_run_assignment(
        &self,
        owner_agent_id: AgentId,
        run_id: RunId,
    ) -> Result<daruma_shared::PlanId> {
        let row = sqlx::query(
            "SELECT r.plan_id, r.status, p.status AS plan_status, \
                    o.agent_id AS owner_agent_id \
             FROM runs r JOIN plans p ON p.id = r.plan_id \
             LEFT JOIN run_claim_owners o ON o.run_id = r.id WHERE r.id = ?",
        )
        .bind(run_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        validated_run_plan_id(row, &run_id.to_string(), &owner_agent_id.to_string())?
            .parse()
            .map_err(|e| CoreError::storage(format!("invalid persisted plan id: {e}")))
    }

    async fn try_acquire_bound_recorded(
        &self,
        actor: Actor,
        agent_id: AgentId,
        task_id: TaskId,
        claim_id: ClaimId,
        expires_at: Timestamp,
        run: Option<(RunId, daruma_shared::PlanId)>,
    ) -> Result<RecordedClaimOutcome> {
        let now_s = Utc::now().to_rfc3339();
        let expires_s = expires_at.to_rfc3339();
        let run_id = run.map(|(run_id, _)| run_id.to_string());
        let plan_id = run.map(|(_, plan_id)| plan_id.to_string());
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        if let Some(run_id) = run_id.as_deref() {
            let row = sqlx::query(
                "SELECT r.plan_id, r.status, p.status AS plan_status, \
                        o.agent_id AS owner_agent_id \
                 FROM runs r JOIN plans p ON p.id = r.plan_id \
                 LEFT JOIN run_claim_owners o ON o.run_id = r.id WHERE r.id = ?",
            )
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
            let actual_plan_id = validated_run_plan_id(row, run_id, &agent_id.to_string())?;
            if actual_plan_id != plan_id.as_deref().unwrap() {
                return Err(CoreError::conflict("run belongs to a different plan"));
            }
        }

        let changed = sqlx::query(
            "INSERT INTO agent_claims \
                (agent_id, task_id, acquired_at, expires_at, run_id, claim_id) \
             SELECT ?, ?, ?, ?, ?, ? \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM agent_claims \
                 WHERE task_id = ? AND expires_at >= ? AND agent_id <> ? \
             ) \
             ON CONFLICT(agent_id, task_id) DO UPDATE SET \
                 acquired_at = excluded.acquired_at, \
                 expires_at  = excluded.expires_at, \
                 run_id      = CASE \
                     WHEN agent_claims.expires_at >= ? AND excluded.run_id IS NULL \
                     THEN agent_claims.run_id \
                     ELSE excluded.run_id \
                 END, \
                 claim_id    = excluded.claim_id",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .bind(&now_s)
        .bind(&expires_s)
        .bind(run_id.as_deref())
        .bind(claim_id.to_string())
        .bind(task_id.to_string())
        .bind(&now_s)
        .bind(agent_id.to_string())
        .bind(&now_s)
        .execute(&mut *tx)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        if changed.rows_affected() == 1 {
            let event = append_on(
                &mut *tx,
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
            tx.commit()
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
            return Ok(RecordedClaimOutcome::Acquired {
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
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        match row {
            Some(row) => {
                let holder = row
                    .try_get::<String, _>("agent_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?
                    .parse::<AgentId>()
                    .map_err(|e| CoreError::serde(e.to_string()))?;
                let expires_at = parse_ts(
                    &row.try_get::<String, _>("expires_at")
                        .map_err(|e| CoreError::storage(e.to_string()))?,
                )?;
                Ok(RecordedClaimOutcome::Busy { holder, expires_at })
            }
            None => Ok(RecordedClaimOutcome::Busy {
                holder: agent_id,
                expires_at,
            }),
        }
    }

    /// Persist `RunStarted` and its authenticated owner atomically.
    pub async fn record_run_started(
        &self,
        owner_agent_id: AgentId,
        envelopes: Vec<EventEnvelope>,
    ) -> Result<Vec<EventEnvelope>> {
        let run_id = envelopes
            .iter()
            .find_map(|envelope| match &envelope.payload {
                Event::RunStarted { run } => Some(run.id),
                _ => None,
            })
            .ok_or_else(|| CoreError::validation("expected RunStarted event"))?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        sqlx::query("INSERT INTO run_claim_owners (run_id, agent_id) VALUES (?, ?)")
            .bind(run_id.to_string())
            .bind(owner_agent_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let mut persisted = Vec::with_capacity(envelopes.len());
        for envelope in envelopes {
            persisted.push(append_on(&mut *tx, envelope).await?);
        }
        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(persisted)
    }

    /// Authorize and persist `FailRun` / `AbortRun` with claim cleanup.
    /// Run state, exact claim generations, owner deletion, and audit events
    /// share one transaction so a failed append leaves the run untouched.
    pub async fn record_run_terminal(
        &self,
        actor: Actor,
        authenticated_agent_id: AgentId,
        is_admin: bool,
        envelopes: Vec<EventEnvelope>,
    ) -> Result<Vec<EventEnvelope>> {
        let (run_id, status, ended_at, outcome) = envelopes
            .iter()
            .find_map(|envelope| match &envelope.payload {
                Event::RunFailed {
                    run_id, reason, at, ..
                } => Some((*run_id, "failed", at.to_rfc3339(), reason.clone())),
                Event::RunAborted {
                    run_id, reason, at, ..
                } => Some((*run_id, "aborted", at.to_rfc3339(), reason.clone())),
                _ => None,
            })
            .ok_or_else(|| CoreError::validation("expected failed or aborted run event"))?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let row = sqlx::query(
            "SELECT r.status, r.agent_id AS run_agent_id, r.started_at, \
                    o.agent_id AS owner_agent_id \
             FROM runs r LEFT JOIN run_claim_owners o ON o.run_id = r.id \
             WHERE r.id = ?",
        )
        .bind(run_id.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?
        .ok_or_else(|| CoreError::not_found(format!("run {run_id}")))?;

        if row
            .try_get::<String, _>("status")
            .map_err(|e| CoreError::storage(e.to_string()))?
            != "active"
        {
            return Err(CoreError::conflict("run is not active"));
        }

        let owner = row
            .try_get::<Option<String>, _>("owner_agent_id")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        match owner.as_deref() {
            Some(owner) if is_admin || owner == authenticated_agent_id.to_string() => {}
            Some(_) => return Err(CoreError::forbidden("run is owned by another agent")),
            None if is_admin => {}
            None => return Err(CoreError::forbidden("run has no authenticated owner")),
        }

        let updated = sqlx::query(
            "UPDATE runs SET status = ?, ended_at = ?, outcome = ? \
             WHERE id = ? AND status = 'active'",
        )
        .bind(status)
        .bind(&ended_at)
        .bind(&outcome)
        .bind(run_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        if updated.rows_affected() != 1 {
            return Err(CoreError::conflict("run is not active"));
        }

        let claims = if owner.is_some() {
            sqlx::query("SELECT agent_id, task_id, claim_id FROM agent_claims WHERE run_id = ?")
                .bind(run_id.to_string())
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?
        } else {
            // ponytail: pre-0054 rows have no run_id; admin recovery only
            // touches the run agent's claims on this run's open steps.
            let run_agent_id = row
                .try_get::<String, _>("run_agent_id")
                .map_err(|e| CoreError::storage(e.to_string()))?;
            let started_at = row
                .try_get::<String, _>("started_at")
                .map_err(|e| CoreError::storage(e.to_string()))?;
            sqlx::query(
                "SELECT agent_id, task_id, claim_id FROM agent_claims ac \
                 WHERE ac.run_id = ? OR ( \
                     ac.run_id IS NULL AND ac.agent_id = ? AND ac.acquired_at >= ? AND EXISTS ( \
                         SELECT 1 FROM run_steps rs WHERE rs.run_id = ? \
                           AND rs.task_id = ac.task_id AND rs.finished_at IS NULL \
                     ) \
                 )",
            )
            .bind(run_id.to_string())
            .bind(run_agent_id)
            .bind(started_at)
            .bind(run_id.to_string())
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?
        };

        let mut released = Vec::with_capacity(claims.len());
        for row in claims {
            let agent_id = row
                .try_get::<String, _>("agent_id")
                .map_err(|e| CoreError::storage(e.to_string()))?;
            let task_id = row
                .try_get::<String, _>("task_id")
                .map_err(|e| CoreError::storage(e.to_string()))?;
            let claim_id = row
                .try_get::<String, _>("claim_id")
                .map_err(|e| CoreError::storage(e.to_string()))?;
            let deleted = sqlx::query(
                "DELETE FROM agent_claims \
                 WHERE agent_id = ? AND task_id = ? AND claim_id = ?",
            )
            .bind(&agent_id)
            .bind(&task_id)
            .bind(&claim_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
            if deleted.rows_affected() == 1 {
                released.push((agent_id, task_id, claim_id));
            }
        }

        sqlx::query("DELETE FROM run_claim_owners WHERE run_id = ?")
            .bind(run_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        let mut persisted = Vec::with_capacity(envelopes.len() + released.len());
        for envelope in envelopes {
            persisted.push(append_on(&mut *tx, envelope).await?);
        }
        for (agent_id, task_id, claim_id) in released {
            persisted.push(
                append_on(
                    &mut *tx,
                    EventEnvelope::new(
                        actor.clone(),
                        Event::AgentReleased {
                            agent_id: agent_id
                                .parse()
                                .map_err(|e| CoreError::serde(format!("{e}")))?,
                            task_id: task_id
                                .parse()
                                .map_err(|e| CoreError::serde(format!("{e}")))?,
                            claim_id: Some(
                                claim_id
                                    .parse()
                                    .map_err(|e| CoreError::serde(format!("{e}")))?,
                            ),
                        },
                    ),
                )
                .await?,
            );
        }
        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(persisted)
    }

    /// Persist plan archive/delete run cleanup with the plan event batch.
    /// The command handler serializes this boundary against `StartRun`.
    pub async fn record_plan_terminal(
        &self,
        actor: Actor,
        envelopes: Vec<EventEnvelope>,
    ) -> Result<Vec<EventEnvelope>> {
        let (plan_id, status, reason, ended_at) = envelopes
            .iter()
            .find_map(|envelope| match &envelope.payload {
                Event::PlanArchived { plan_id, at } => {
                    Some((*plan_id, "aborted", "plan_archived", at.to_rfc3339()))
                }
                Event::PlanDeleted { plan_id, at } => {
                    Some((*plan_id, "aborted", "plan_deleted", at.to_rfc3339()))
                }
                _ => None,
            })
            .ok_or_else(|| CoreError::validation("expected archived or deleted plan event"))?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        // Acquire SQLite's writer boundary before selecting active runs.
        sqlx::query("UPDATE plans SET updated_at = updated_at WHERE id = ?")
            .bind(plan_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let runs = sqlx::query("SELECT id FROM runs WHERE plan_id = ? AND status = 'active'")
            .bind(plan_id.to_string())
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let mut released = Vec::new();

        for run in runs {
            let run_id = run
                .try_get::<String, _>("id")
                .map_err(|e| CoreError::storage(e.to_string()))?;
            let claims = sqlx::query(
                "SELECT agent_id, task_id, claim_id FROM agent_claims WHERE run_id = ?",
            )
            .bind(&run_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

            sqlx::query(
                "UPDATE runs SET status = ?, ended_at = ?, outcome = ? \
                 WHERE id = ? AND status = 'active'",
            )
            .bind(status)
            .bind(&ended_at)
            .bind(reason)
            .bind(&run_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

            for claim in claims {
                let agent_id = claim
                    .try_get::<String, _>("agent_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let task_id = claim
                    .try_get::<String, _>("task_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let claim_id = claim
                    .try_get::<String, _>("claim_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let deleted = sqlx::query(
                    "DELETE FROM agent_claims \
                     WHERE agent_id = ? AND task_id = ? AND claim_id = ?",
                )
                .bind(&agent_id)
                .bind(&task_id)
                .bind(&claim_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
                if deleted.rows_affected() == 1 {
                    released.push((agent_id, task_id, claim_id));
                }
            }
            sqlx::query("DELETE FROM run_claim_owners WHERE run_id = ?")
                .bind(&run_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
        }

        let mut persisted = Vec::with_capacity(envelopes.len() + released.len());
        for envelope in envelopes {
            persisted.push(append_on(&mut *tx, envelope).await?);
        }
        for (agent_id, task_id, claim_id) in released {
            persisted.push(
                append_on(
                    &mut *tx,
                    EventEnvelope::new(
                        actor.clone(),
                        Event::AgentReleased {
                            agent_id: agent_id
                                .parse()
                                .map_err(|e| CoreError::serde(format!("{e}")))?,
                            task_id: task_id
                                .parse()
                                .map_err(|e| CoreError::serde(format!("{e}")))?,
                            claim_id: Some(
                                claim_id
                                    .parse()
                                    .map_err(|e| CoreError::serde(format!("{e}")))?,
                            ),
                        },
                    ),
                )
                .await?,
            );
        }
        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(persisted)
    }

    /// Persist task close/delete events together with generation-conditional
    /// claim cleanup. The transaction takes SQLite's writer boundary before it
    /// snapshots current generations, so a concurrent refresh serializes either
    /// before cleanup (and is released as the current generation) or after the
    /// lifecycle commit (and cannot be erased by delayed projector work).
    pub async fn record_task_lifecycle(
        &self,
        actor: Actor,
        envelopes: Vec<EventEnvelope>,
    ) -> Result<Vec<EventEnvelope>> {
        let mut cleanup_tasks = Vec::new();
        for envelope in &envelopes {
            let task_id = match &envelope.payload {
                Event::TaskClosed { task_id, .. } | Event::TaskDeleted { task_id } => {
                    Some(*task_id)
                }
                _ => None,
            };
            if let Some(task_id) = task_id {
                if !cleanup_tasks.contains(&task_id) {
                    cleanup_tasks.push(task_id);
                }
            }
        }

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let mut released = Vec::new();
        for task_id in cleanup_tasks {
            // A no-op UPDATE acquires the write boundary even when no claim row
            // exists. Claim refresh/reacquire therefore cannot race the select.
            sqlx::query("UPDATE agent_claims SET expires_at = expires_at WHERE task_id = ?")
                .bind(task_id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;

            let claims =
                sqlx::query("SELECT agent_id, claim_id FROM agent_claims WHERE task_id = ?")
                    .bind(task_id.to_string())
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            for claim in claims {
                let agent_id = claim
                    .try_get::<String, _>("agent_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let claim_id = claim
                    .try_get::<String, _>("claim_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let deleted = sqlx::query(
                    "DELETE FROM agent_claims \
                     WHERE agent_id = ? AND task_id = ? AND claim_id = ?",
                )
                .bind(&agent_id)
                .bind(task_id.to_string())
                .bind(&claim_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
                if deleted.rows_affected() == 1 {
                    released.push((agent_id, task_id, claim_id));
                }
            }
        }

        let mut persisted = Vec::with_capacity(envelopes.len() + released.len());
        for envelope in envelopes {
            persisted.push(append_on(&mut *tx, envelope).await?);
        }
        for (agent_id, task_id, claim_id) in released {
            persisted.push(
                append_on(
                    &mut *tx,
                    EventEnvelope::new(
                        actor.clone(),
                        Event::AgentReleased {
                            agent_id: agent_id
                                .parse()
                                .map_err(|e| CoreError::serde(format!("{e}")))?,
                            task_id,
                            claim_id: Some(
                                claim_id
                                    .parse()
                                    .map_err(|e| CoreError::serde(format!("{e}")))?,
                            ),
                        },
                    ),
                )
                .await?,
            );
        }
        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(persisted)
    }

    /// Release a specific agent's claim on a task. Legacy helper for callers
    /// that do not participate in generation-aware command/audit handling.
    pub async fn release(&self, agent_id: AgentId, task_id: TaskId) -> Result<()> {
        sqlx::query("DELETE FROM agent_claims WHERE agent_id = ? AND task_id = ?")
            .bind(agent_id.to_string())
            .bind(task_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Release exactly one claim generation and append `AgentReleased` in the
    /// same transaction. A stale generation is a truthful no-op.
    pub async fn release_recorded(
        &self,
        actor: Actor,
        agent_id: AgentId,
        task_id: TaskId,
        claim_id: ClaimId,
    ) -> Result<Option<EventEnvelope>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let deleted = sqlx::query(
            "DELETE FROM agent_claims WHERE agent_id = ? AND task_id = ? AND claim_id = ?",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .bind(claim_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        if deleted.rows_affected() == 0 {
            tx.commit()
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
            return Ok(None);
        }

        let event = append_on(
            &mut *tx,
            EventEnvelope::new(
                actor,
                Event::AgentReleased {
                    agent_id,
                    task_id,
                    claim_id: Some(claim_id),
                },
            ),
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(Some(event))
    }

    /// Apply a persisted event to the projection. Task close/delete cleanup is
    /// committed by [`Self::record_task_lifecycle`]. Lifecycle replay is a no-op
    /// here so a delayed projector cannot erase a later claim generation.
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
                claim_id: Some(claim_id),
            } => {
                sqlx::query(
                    "DELETE FROM agent_claims WHERE agent_id = ? AND task_id = ? AND claim_id = ?",
                )
                .bind(agent_id.to_string())
                .bind(task_id.to_string())
                .bind(claim_id.to_string())
                .execute(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(())
            }
            Event::AgentReleased {
                agent_id,
                task_id,
                claim_id: None,
            } => self.release(*agent_id, *task_id).await,
            // Task lifecycle cleanup is already generation-conditional and
            // atomic with event persistence. Replaying delayed close/reopen/
            // delete events must not touch a later generation.
            Event::TaskClosed { .. } | Event::TaskReopened { .. } | Event::TaskDeleted { .. } => {
                Ok(())
            }
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

    /// Sweep expired generations with conditional delete and audit append in
    /// one transaction per batch.
    pub async fn sweep_expired_recorded(&self, actor: Actor) -> Result<Vec<EventEnvelope>> {
        self.sweep_expired_recorded_after(actor, || async {}).await
    }

    async fn sweep_expired_recorded_after<F, Fut>(
        &self,
        actor: Actor,
        after_select: F,
    ) -> Result<Vec<EventEnvelope>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = ()>,
    {
        let now = Utc::now().to_rfc3339();
        let rows = sqlx::query(
            "SELECT agent_id, task_id, claim_id FROM agent_claims WHERE expires_at < ?",
        )
        .bind(&now)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        after_select().await;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
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
            .execute(&mut *tx)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
            if deleted.rows_affected() == 1 {
                events.push(
                    append_on(
                        &mut *tx,
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
        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(events)
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn validated_run_plan_id(
    row: Option<sqlx::sqlite::SqliteRow>,
    run_id: &str,
    owner_agent_id: &str,
) -> Result<String> {
    let row = row.ok_or_else(|| CoreError::not_found(format!("run {run_id}")))?;
    let plan_id = row
        .try_get::<String, _>("plan_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    if row
        .try_get::<String, _>("status")
        .map_err(|e| CoreError::storage(e.to_string()))?
        != "active"
    {
        return Err(CoreError::conflict("run is not active"));
    }
    if row
        .try_get::<String, _>("plan_status")
        .map_err(|e| CoreError::storage(e.to_string()))?
        != "active"
    {
        return Err(CoreError::conflict("run plan is not active"));
    }
    if row
        .try_get::<Option<String>, _>("owner_agent_id")
        .map_err(|e| CoreError::storage(e.to_string()))?
        .as_deref()
        != Some(owner_agent_id)
    {
        return Err(CoreError::forbidden(
            "run has no matching authenticated owner",
        ));
    }
    Ok(plan_id)
}

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
    use daruma_domain::Actor;
    use daruma_shared::{AgentId, TaskId};
    use std::sync::Arc;
    use tokio::sync::Notify;

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
    async fn failed_audit_after_acquire_leaves_neither_row_nor_event() {
        let (db, repo) = make_repo().await;
        let agent_id = AgentId::new();
        let task_id = TaskId::new();

        sqlx::query(
            "CREATE TRIGGER fail_claim_audit BEFORE INSERT ON events \
             WHEN NEW.kind = 'agent_claimed' BEGIN \
             SELECT RAISE(ABORT, 'forced claim audit failure'); END",
        )
        .execute(db.pool())
        .await
        .unwrap();

        assert!(repo
            .try_acquire_recorded(Actor::user(), agent_id, task_id, Duration::seconds(60),)
            .await
            .is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM agent_claims WHERE agent_id = ? AND task_id = ?",
            )
            .bind(agent_id.to_string())
            .bind(task_id.to_string())
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
    async fn failed_release_audit_preserves_exact_claim_and_emits_nothing() {
        let (db, repo) = make_repo().await;
        let agent_id = AgentId::new();
        let task_id = TaskId::new();
        let claim_id = match repo
            .try_acquire_recorded(Actor::user(), agent_id, task_id, Duration::seconds(60))
            .await
            .unwrap()
        {
            RecordedClaimOutcome::Acquired { claim_id, .. } => claim_id,
            other => panic!("expected acquired claim, got {other:?}"),
        };

        sqlx::query(
            "CREATE TRIGGER fail_release_audit BEFORE INSERT ON events \
             WHEN NEW.kind = 'agent_released' BEGIN \
             SELECT RAISE(ABORT, 'forced release audit failure'); END",
        )
        .execute(db.pool())
        .await
        .unwrap();

        assert!(repo
            .release_recorded(Actor::user(), agent_id, task_id, claim_id)
            .await
            .is_err());
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT claim_id FROM agent_claims WHERE agent_id = ? AND task_id = ?",
            )
            .bind(agent_id.to_string())
            .bind(task_id.to_string())
            .fetch_one(db.pool())
            .await
            .unwrap(),
            claim_id.to_string()
        );
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
    async fn delayed_compensation_cannot_release_reacquired_generation() {
        let (db, repo) = make_repo().await;
        let agent_id = AgentId::new();
        let task_id = TaskId::new();
        let stale_claim_id = match repo
            .try_acquire_recorded(Actor::user(), agent_id, task_id, Duration::seconds(60))
            .await
            .unwrap()
        {
            RecordedClaimOutcome::Acquired { claim_id, .. } => claim_id,
            other => panic!("expected acquired claim, got {other:?}"),
        };

        let compensation_ready = Arc::new(Notify::new());
        let compensation_resume = Arc::new(Notify::new());
        let ready = compensation_ready.clone();
        let resume = compensation_resume.clone();
        let compensation_repo = AgentClaimRepo::new(db.pool().clone());
        let compensation = tokio::spawn(async move {
            ready.notify_one();
            resume.notified().await;
            compensation_repo
                .release_recorded(Actor::user(), agent_id, task_id, stale_claim_id)
                .await
                .unwrap()
        });

        compensation_ready.notified().await;
        let current_claim_id = match repo
            .try_acquire_recorded(Actor::user(), agent_id, task_id, Duration::seconds(120))
            .await
            .unwrap()
        {
            RecordedClaimOutcome::Acquired { claim_id, .. } => claim_id,
            other => panic!("expected reacquired claim, got {other:?}"),
        };
        assert_ne!(current_claim_id, stale_claim_id);
        compensation_resume.notify_one();

        assert!(compensation.await.unwrap().is_none());
        let active = repo.list_active(None).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].claim_id, current_claim_id);
    }

    #[tokio::test]
    async fn task_close_records_exact_generation_release_atomically() {
        let (db, repo) = make_repo().await;
        let agent_id = AgentId::new();
        let task_id = TaskId::new();
        let claim_id = match repo
            .try_acquire_recorded(Actor::user(), agent_id, task_id, Duration::seconds(60))
            .await
            .unwrap()
        {
            RecordedClaimOutcome::Acquired { claim_id, .. } => claim_id,
            other => panic!("expected acquired claim, got {other:?}"),
        };
        let at = Utc::now();

        let persisted = repo
            .record_task_lifecycle(
                Actor::user(),
                vec![EventEnvelope::new(
                    Actor::user(),
                    Event::TaskClosed {
                        task_id,
                        by: Actor::user(),
                        at,
                    },
                )],
            )
            .await
            .unwrap();

        assert_eq!(persisted.len(), 2);
        assert!(matches!(
            persisted[0].payload,
            Event::TaskClosed { task_id: id, .. } if id == task_id
        ));
        assert!(matches!(
            persisted[1].payload,
            Event::AgentReleased {
                agent_id: a,
                task_id: t,
                claim_id: Some(generation),
            } if a == agent_id && t == task_id && generation == claim_id
        ));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_claims WHERE task_id = ?",)
                .bind(task_id.to_string())
                .fetch_one(db.pool())
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM events WHERE kind IN ('task_closed', 'agent_released')",
            )
            .fetch_one(db.pool())
            .await
            .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn delayed_close_projector_cannot_delete_reopened_generation() {
        let (db, repo) = make_repo().await;
        let agent_id = AgentId::new();
        let task_id = TaskId::new();
        let old_claim_id = match repo
            .try_acquire_recorded(Actor::user(), agent_id, task_id, Duration::seconds(60))
            .await
            .unwrap()
        {
            RecordedClaimOutcome::Acquired { claim_id, .. } => claim_id,
            other => panic!("expected acquired claim, got {other:?}"),
        };
        let lifecycle = repo
            .record_task_lifecycle(
                Actor::user(),
                vec![EventEnvelope::new(
                    Actor::user(),
                    Event::TaskClosed {
                        task_id,
                        by: Actor::user(),
                        at: Utc::now(),
                    },
                )],
            )
            .await
            .unwrap();

        let projector_ready = Arc::new(Notify::new());
        let projector_resume = Arc::new(Notify::new());
        let ready = projector_ready.clone();
        let resume = projector_resume.clone();
        let replay_repo = AgentClaimRepo::new(db.pool().clone());
        let delayed_projector = tokio::spawn(async move {
            ready.notify_one();
            resume.notified().await;
            for event in &lifecycle {
                replay_repo.apply_event(event).await.unwrap();
            }
        });

        projector_ready.notified().await;
        let reopened_claim_id = match repo
            .try_acquire_recorded(Actor::user(), agent_id, task_id, Duration::seconds(120))
            .await
            .unwrap()
        {
            RecordedClaimOutcome::Acquired { claim_id, .. } => claim_id,
            other => panic!("expected reopened claim, got {other:?}"),
        };
        assert_ne!(reopened_claim_id, old_claim_id);
        projector_resume.notify_one();
        delayed_projector.await.unwrap();

        let active = repo.list_active(None).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].claim_id, reopened_claim_id);
    }

    #[tokio::test]
    async fn sweep_selection_cannot_delete_concurrent_refresh_or_emit_release() {
        let (db, repo) = make_repo().await;
        let agent_id = AgentId::new();
        let task_id = TaskId::new();
        let stale_claim_id = ClaimId::new();

        sqlx::query(
            "INSERT INTO agent_claims (agent_id, task_id, acquired_at, expires_at, claim_id) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .bind("2000-01-01T00:00:00+00:00")
        .bind(stale_claim_id.to_string())
        .execute(db.pool())
        .await
        .unwrap();

        let selected = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let sweep_selected = selected.clone();
        let sweep_resume = resume.clone();
        let sweep = tokio::spawn(async move {
            repo.sweep_expired_recorded_after(Actor::user(), || async move {
                sweep_selected.notify_one();
                sweep_resume.notified().await;
            })
            .await
        });

        selected.notified().await;
        let refresh_repo = AgentClaimRepo::new(db.pool().clone());
        let refreshed = refresh_repo
            .try_acquire_recorded(Actor::user(), agent_id, task_id, Duration::seconds(60))
            .await
            .unwrap();
        let refreshed_claim_id = match refreshed {
            RecordedClaimOutcome::Acquired { claim_id, .. } => claim_id,
            other => panic!("expected refreshed claim, got {other:?}"),
        };
        resume.notify_one();

        assert!(sweep.await.unwrap().unwrap().is_empty());
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT claim_id FROM agent_claims WHERE agent_id = ? AND task_id = ?",
            )
            .bind(agent_id.to_string())
            .bind(task_id.to_string())
            .fetch_one(db.pool())
            .await
            .unwrap(),
            refreshed_claim_id.to_string()
        );
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
    async fn live_run_bound_claim_keeps_run_on_runless_refresh() {
        let (db, repo) = make_repo().await;
        let agent_id = AgentId::new();
        let task_id = TaskId::new();
        let run_id = RunId::new();
        let old_claim_id = ClaimId::new();

        sqlx::query(
            "INSERT INTO agent_claims \
             (agent_id, task_id, acquired_at, expires_at, run_id, claim_id) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .bind((Utc::now() + Duration::seconds(60)).to_rfc3339())
        .bind(run_id.to_string())
        .bind(old_claim_id.to_string())
        .execute(db.pool())
        .await
        .unwrap();

        let refreshed_claim_id = match repo
            .try_acquire_recorded(Actor::user(), agent_id, task_id, Duration::seconds(120))
            .await
            .unwrap()
        {
            RecordedClaimOutcome::Acquired { claim_id, .. } => claim_id,
            other => panic!("expected refreshed claim, got {other:?}"),
        };

        let (persisted_run_id, persisted_claim_id): (Option<String>, String) = sqlx::query_as(
            "SELECT run_id, claim_id FROM agent_claims WHERE agent_id = ? AND task_id = ?",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(
            persisted_run_id.as_deref(),
            Some(run_id.to_string().as_str())
        );
        assert_eq!(persisted_claim_id, refreshed_claim_id.to_string());
        assert_ne!(persisted_claim_id, old_claim_id.to_string());
    }

    #[tokio::test]
    async fn expired_run_bound_claim_drops_run_on_runless_reacquire() {
        let (db, repo) = make_repo().await;
        let agent_id = AgentId::new();
        let task_id = TaskId::new();
        let stale_run_id = RunId::new();
        let stale_claim_id = ClaimId::new();

        sqlx::query(
            "INSERT INTO agent_claims \
             (agent_id, task_id, acquired_at, expires_at, run_id, claim_id) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .bind((Utc::now() - Duration::seconds(120)).to_rfc3339())
        .bind((Utc::now() - Duration::seconds(60)).to_rfc3339())
        .bind(stale_run_id.to_string())
        .bind(stale_claim_id.to_string())
        .execute(db.pool())
        .await
        .unwrap();

        let refreshed_claim_id = match repo
            .try_acquire(agent_id, task_id, Duration::seconds(120))
            .await
            .unwrap()
        {
            ClaimOutcome::Acquired { claim_id, .. } => claim_id,
            other => panic!("expected reacquired claim, got {other:?}"),
        };

        let (persisted_run_id, persisted_claim_id): (Option<String>, String) = sqlx::query_as(
            "SELECT run_id, claim_id FROM agent_claims WHERE agent_id = ? AND task_id = ?",
        )
        .bind(agent_id.to_string())
        .bind(task_id.to_string())
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(persisted_run_id, None);
        assert_eq!(persisted_claim_id, refreshed_claim_id.to_string());
        assert_ne!(persisted_claim_id, stale_claim_id.to_string());
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
    async fn migration_0054_preserves_live_claims_and_backfills_unique_generations() {
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

        sqlx::raw_sql(include_str!("../migrations/0054_claim_run_ownership.sql"))
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
            assert!(row.4.is_none(), "pre-0054 claims have no safe run binding");
            uuid::Uuid::parse_str(
                row.5
                    .strip_prefix("clm_")
                    .expect("claim generation uses the typed clm_ UUID form"),
            )
            .expect("claim generation must contain a UUID");
        }
        assert_ne!(rows[0].5, rows[1].5);
    }

    async fn insert_active_run(
        db: &Db,
        run_id: RunId,
        plan_id: daruma_shared::PlanId,
        agent_id: AgentId,
    ) {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO runs \
             (id, plan_id, agent_id, started_at, status, last_activity_at) \
             VALUES (?, ?, ?, ?, 'active', ?)",
        )
        .bind(run_id.to_string())
        .bind(plan_id.to_string())
        .bind(agent_id.to_string())
        .bind(&now)
        .bind(&now)
        .execute(db.pool())
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn plan_terminal_cleans_active_run_owner_and_claim_atomically() {
        let (db, repo) = make_repo().await;
        let plan_id = daruma_shared::PlanId::new();
        let run_id = RunId::new();
        let owner = AgentId::new();
        let task_id = TaskId::new();
        let claim_id = ClaimId::new();
        insert_active_run(&db, run_id, plan_id, owner).await;
        sqlx::query("INSERT INTO run_claim_owners (run_id, agent_id) VALUES (?, ?)")
            .bind(run_id.to_string())
            .bind(owner.to_string())
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO agent_claims \
             (agent_id, task_id, acquired_at, expires_at, run_id, claim_id) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(owner.to_string())
        .bind(task_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .bind((Utc::now() + Duration::minutes(5)).to_rfc3339())
        .bind(run_id.to_string())
        .bind(claim_id.to_string())
        .execute(db.pool())
        .await
        .unwrap();
        let at = Utc::now();
        let events = vec![
            EventEnvelope::new(Actor::user(), Event::PlanArchived { plan_id, at }),
            EventEnvelope::new(
                Actor::user(),
                Event::RunAborted {
                    run_id,
                    reason: "plan_archived".into(),
                    at,
                },
            ),
            EventEnvelope::new(
                Actor::user(),
                Event::RunObsolescedByPlanEdit {
                    run_id,
                    plan_id,
                    kind: daruma_events::event::ObsolescenceKind::Archived,
                },
            ),
        ];

        let persisted = repo
            .record_plan_terminal(Actor::user(), events)
            .await
            .unwrap();

        assert!(persisted.iter().any(|event| matches!(
            &event.payload,
            Event::AgentReleased {
                claim_id: Some(id), ..
            } if *id == claim_id
        )));
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM runs WHERE id = ?")
                .bind(run_id.to_string())
                .fetch_one(db.pool())
                .await
                .unwrap(),
            "aborted"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM run_claim_owners WHERE run_id = ?",)
                .bind(run_id.to_string())
                .fetch_one(db.pool())
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_claims WHERE run_id = ?")
                .bind(run_id.to_string())
                .fetch_one(db.pool())
                .await
                .unwrap(),
            0
        );
    }

    fn failed_event(run_id: RunId) -> Vec<EventEnvelope> {
        vec![EventEnvelope::new(
            Actor::user(),
            Event::RunFailed {
                run_id,
                reason: "test failure".into(),
                at: Utc::now(),
            },
        )]
    }

    fn aborted_event(run_id: RunId) -> Vec<EventEnvelope> {
        vec![EventEnvelope::new(
            Actor::user(),
            Event::RunAborted {
                run_id,
                reason: "test abort".into(),
                at: Utc::now(),
            },
        )]
    }

    #[tokio::test]
    async fn owner_fails_run_and_cleans_exact_claim_and_owner_atomically() {
        let (db, repo) = make_repo().await;
        let run_id = RunId::new();
        let owner = AgentId::new();
        let task_id = TaskId::new();
        let claim_id = ClaimId::new();
        insert_active_run(&db, run_id, daruma_shared::PlanId::new(), owner).await;
        sqlx::query("INSERT INTO run_claim_owners (run_id, agent_id) VALUES (?, ?)")
            .bind(run_id.to_string())
            .bind(owner.to_string())
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO agent_claims \
             (agent_id, task_id, acquired_at, expires_at, run_id, claim_id) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(owner.to_string())
        .bind(task_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .bind((Utc::now() + Duration::minutes(5)).to_rfc3339())
        .bind(run_id.to_string())
        .bind(claim_id.to_string())
        .execute(db.pool())
        .await
        .unwrap();

        let error = repo
            .record_run_terminal(Actor::user(), AgentId::new(), false, failed_event(run_id))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "forbidden");
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM runs WHERE id = ?")
                .bind(run_id.to_string())
                .fetch_one(db.pool())
                .await
                .unwrap(),
            "active"
        );

        let events = repo
            .record_run_terminal(Actor::user(), owner, false, failed_event(run_id))
            .await
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            Event::AgentReleased {
                claim_id: Some(id), ..
            } if *id == claim_id
        )));
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM runs WHERE id = ?")
                .bind(run_id.to_string())
                .fetch_one(db.pool())
                .await
                .unwrap(),
            "failed"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_claims WHERE run_id = ?")
                .bind(run_id.to_string())
                .fetch_one(db.pool())
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM run_claim_owners WHERE run_id = ?",)
                .bind(run_id.to_string())
                .fetch_one(db.pool())
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn admin_recovers_only_bounded_migrated_ownerless_claims() {
        let (db, repo) = make_repo().await;
        let run_id = RunId::new();
        let legacy_agent = AgentId::new();
        let task_id = TaskId::new();
        let unrelated_task_id = TaskId::new();
        let legacy_claim_id = ClaimId::new();
        let unrelated_claim_id = ClaimId::new();
        insert_active_run(&db, run_id, daruma_shared::PlanId::new(), legacy_agent).await;
        for (task_id, claim_id) in [
            (task_id, legacy_claim_id),
            (unrelated_task_id, unrelated_claim_id),
        ] {
            sqlx::query(
                "INSERT INTO agent_claims \
                 (agent_id, task_id, acquired_at, expires_at, run_id, claim_id) \
                 VALUES (?, ?, ?, ?, NULL, ?)",
            )
            .bind(legacy_agent.to_string())
            .bind(task_id.to_string())
            .bind(Utc::now().to_rfc3339())
            .bind((Utc::now() + Duration::minutes(5)).to_rfc3339())
            .bind(claim_id.to_string())
            .execute(db.pool())
            .await
            .unwrap();
        }
        sqlx::query("INSERT INTO run_steps (run_id, task_id, started_at) VALUES (?, ?, ?)")
            .bind(run_id.to_string())
            .bind(task_id.to_string())
            .bind(Utc::now().to_rfc3339())
            .execute(db.pool())
            .await
            .unwrap();

        let error = repo
            .record_run_terminal(Actor::user(), legacy_agent, false, aborted_event(run_id))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "forbidden");

        let events = repo
            .record_run_terminal(Actor::user(), AgentId::new(), true, aborted_event(run_id))
            .await
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            Event::AgentReleased {
                claim_id: Some(id), ..
            } if *id == legacy_claim_id
        )));
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT claim_id FROM agent_claims WHERE task_id = ?",)
                .bind(unrelated_task_id.to_string())
                .fetch_one(db.pool())
                .await
                .unwrap(),
            unrelated_claim_id.to_string()
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM runs WHERE id = ?")
                .bind(run_id.to_string())
                .fetch_one(db.pool())
                .await
                .unwrap(),
            "aborted"
        );
    }

    #[tokio::test]
    async fn failed_terminal_audit_rolls_back_run_claim_and_owner() {
        let (db, repo) = make_repo().await;
        let run_id = RunId::new();
        let owner = AgentId::new();
        let task_id = TaskId::new();
        let claim_id = ClaimId::new();
        insert_active_run(&db, run_id, daruma_shared::PlanId::new(), owner).await;
        sqlx::query("INSERT INTO run_claim_owners (run_id, agent_id) VALUES (?, ?)")
            .bind(run_id.to_string())
            .bind(owner.to_string())
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO agent_claims \
             (agent_id, task_id, acquired_at, expires_at, run_id, claim_id) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(owner.to_string())
        .bind(task_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .bind((Utc::now() + Duration::minutes(5)).to_rfc3339())
        .bind(run_id.to_string())
        .bind(claim_id.to_string())
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER fail_terminal_event BEFORE INSERT ON events \
             WHEN NEW.kind = 'run_aborted' BEGIN \
             SELECT RAISE(ABORT, 'forced terminal failure'); END",
        )
        .execute(db.pool())
        .await
        .unwrap();

        assert!(repo
            .record_run_terminal(Actor::user(), owner, false, aborted_event(run_id))
            .await
            .is_err());
        assert_eq!(
            sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
                "SELECT status, ended_at, outcome FROM runs WHERE id = ?",
            )
            .bind(run_id.to_string())
            .fetch_one(db.pool())
            .await
            .unwrap(),
            ("active".into(), None, None)
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT claim_id FROM agent_claims WHERE task_id = ?",)
                .bind(task_id.to_string())
                .fetch_one(db.pool())
                .await
                .unwrap(),
            claim_id.to_string()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM run_claim_owners WHERE run_id = ?",)
                .bind(run_id.to_string())
                .fetch_one(db.pool())
                .await
                .unwrap(),
            1
        );
    }
}
