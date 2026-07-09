//! Run projection repository — materialises run-related events into the
//! `runs` SQLite table.

use crate::parse_ts;
use daruma_domain::{Run, RunStatus};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{AgentId, CoreError, PlanId, Result, RunId, Timestamp};
use sqlx::{Row, SqlitePool};

/// Read/write access to the `runs` projection table.
pub struct RunRepo {
    pub(crate) pool: SqlitePool,
}

impl RunRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── queries ──────────────────────────────────────────────────────────────

    pub async fn get(&self, id: RunId) -> Result<Option<Run>> {
        let row = sqlx::query(
            "SELECT id, plan_id, agent_id, parent_run_id, \
             started_at, ended_at, status, outcome, \
             last_activity_at, unresponsive_at, stale_at \
             FROM runs WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(row_to_run).transpose()
    }

    pub async fn list_by_plan(&self, plan_id: PlanId) -> Result<Vec<Run>> {
        let rows = sqlx::query(
            "SELECT id, plan_id, agent_id, parent_run_id, \
             started_at, ended_at, status, outcome, \
             last_activity_at, unresponsive_at, stale_at \
             FROM runs WHERE plan_id = ? ORDER BY started_at ASC",
        )
        .bind(plan_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_run).collect()
    }

    pub async fn list_active_for_plan(&self, plan_id: PlanId) -> Result<Vec<Run>> {
        let rows = sqlx::query(
            "SELECT id, plan_id, agent_id, parent_run_id, \
             started_at, ended_at, status, outcome, \
             last_activity_at, unresponsive_at, stale_at \
             FROM runs WHERE plan_id = ? AND status = 'active' ORDER BY started_at ASC",
        )
        .bind(plan_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_run).collect()
    }

    // ── §3.7.4 liveness ──────────────────────────────────────────────────────

    /// Candidates for `RunUnresponsive`: active runs that have not yet emitted
    /// the signal, where no `RunStepStarted` has arrived (`last_activity_at`
    /// still equals `started_at`), and `started_at` is at least `threshold`
    /// in the past relative to `now`.
    pub async fn list_unresponsive_candidates(
        &self,
        threshold: std::time::Duration,
        now: Timestamp,
    ) -> Result<Vec<RunId>> {
        let cutoff =
            now - chrono::Duration::from_std(threshold).unwrap_or(chrono::Duration::zero());
        let rows = sqlx::query(
            "SELECT id FROM runs \
             WHERE status = 'active' \
               AND unresponsive_at IS NULL \
               AND last_activity_at IS NOT NULL \
               AND last_activity_at = started_at \
               AND started_at <= ?",
        )
        .bind(cutoff.to_rfc3339())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter()
            .map(|row| {
                let id: String = row
                    .try_get("id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                id.parse::<RunId>()
                    .map_err(|e| CoreError::serde(e.to_string()))
            })
            .collect()
    }

    /// Candidates for `RunStale`: active runs that have not yet emitted the
    /// signal and whose `last_activity_at` is at least `threshold` in the
    /// past relative to `now`.
    pub async fn list_stale_candidates(
        &self,
        threshold: std::time::Duration,
        now: Timestamp,
    ) -> Result<Vec<RunId>> {
        let cutoff =
            now - chrono::Duration::from_std(threshold).unwrap_or(chrono::Duration::zero());
        let rows = sqlx::query(
            "SELECT id FROM runs \
             WHERE status = 'active' \
               AND stale_at IS NULL \
               AND last_activity_at IS NOT NULL \
               AND last_activity_at <= ?",
        )
        .bind(cutoff.to_rfc3339())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter()
            .map(|row| {
                let id: String = row
                    .try_get("id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                id.parse::<RunId>()
                    .map_err(|e| CoreError::serde(e.to_string()))
            })
            .collect()
    }

    async fn touch_activity(&self, run_id: RunId, at: Timestamp) -> Result<()> {
        sqlx::query("UPDATE runs SET last_activity_at = ? WHERE id = ?")
            .bind(at.to_rfc3339())
            .bind(run_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    async fn mark_unresponsive(&self, run_id: RunId, at: Timestamp) -> Result<()> {
        sqlx::query("UPDATE runs SET unresponsive_at = ? WHERE id = ? AND unresponsive_at IS NULL")
            .bind(at.to_rfc3339())
            .bind(run_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    async fn mark_stale(&self, run_id: RunId, at: Timestamp) -> Result<()> {
        sqlx::query("UPDATE runs SET stale_at = ? WHERE id = ? AND stale_at IS NULL")
            .bind(at.to_rfc3339())
            .bind(run_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Return the task currently being executed by the run.
    ///
    /// The `runs` table does not yet track the active step — returns `None`
    /// until a `current_task_id` column is added in a future migration.
    pub async fn current_step_task(&self, _run_id: RunId) -> Result<Option<daruma_shared::TaskId>> {
        Ok(None)
    }

    // ── mutations ────────────────────────────────────────────────────────────

    pub async fn start(&self, run: &Run) -> Result<()> {
        self.upsert_run(run).await
    }

    /// No-op projection side: step tracking is done via events only.
    /// The run stays `active` through steps.
    pub async fn start_step(
        &self,
        _run_id: RunId,
        _task_id: daruma_shared::TaskId,
        _at: Timestamp,
    ) -> Result<()> {
        Ok(())
    }

    /// No-op projection side: outcome is recorded on terminal transitions only.
    pub async fn finish_step(
        &self,
        _run_id: RunId,
        _task_id: daruma_shared::TaskId,
        _outcome: daruma_domain::RunOutcome,
        _at: Timestamp,
    ) -> Result<()> {
        Ok(())
    }

    pub async fn complete(&self, run_id: RunId, at: Timestamp) -> Result<()> {
        sqlx::query(
            "UPDATE runs SET status = 'completed', ended_at = ?, outcome = 'done' WHERE id = ?",
        )
        .bind(at.to_rfc3339())
        .bind(run_id.to_string())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    pub async fn fail(&self, run_id: RunId, reason: &str, at: Timestamp) -> Result<()> {
        sqlx::query("UPDATE runs SET status = 'failed', ended_at = ?, outcome = ? WHERE id = ?")
            .bind(at.to_rfc3339())
            .bind(reason)
            .bind(run_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    pub async fn abort(&self, run_id: RunId, reason: &str, at: Timestamp) -> Result<()> {
        sqlx::query("UPDATE runs SET status = 'aborted', ended_at = ?, outcome = ? WHERE id = ?")
            .bind(at.to_rfc3339())
            .bind(reason)
            .bind(run_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    // ── event application ────────────────────────────────────────────────────

    pub async fn apply_event(&self, envelope: &EventEnvelope) -> Result<()> {
        let occurred_at = envelope.occurred_at;

        match &envelope.payload {
            Event::RunStarted { run } => {
                self.upsert_run(run).await?;
            }

            Event::RunStepStarted { run_id, at, .. }
            | Event::RunStepFinished { run_id, at, .. } => {
                self.touch_activity(*run_id, *at).await?;
            }

            Event::RunCompleted { run_id, at } => {
                self.complete(*run_id, *at).await?;
                let _ = occurred_at; // suppress unused warning
            }

            Event::RunFailed {
                run_id, reason, at, ..
            } => {
                self.fail(*run_id, reason, *at).await?;
            }

            Event::RunAborted {
                run_id, reason, at, ..
            } => {
                self.abort(*run_id, reason, *at).await?;
            }

            Event::RunUnresponsive { run_id, at } => {
                self.mark_unresponsive(*run_id, *at).await?;
            }

            Event::RunStale { run_id, at } => {
                self.mark_stale(*run_id, *at).await?;
            }

            // Semantic events that reference runs but don't change run status.
            Event::RunObsolescedByPlanEdit { .. }
            | Event::RunStopRequested { .. }
            | Event::RunElicitationRequested { .. }
            | Event::RunAuthRequired { .. }
            | Event::RunInterventionAccepted { .. } => {}

            _ => {}
        }

        Ok(())
    }

    // ── private helpers ──────────────────────────────────────────────────────

    async fn upsert_run(&self, run: &Run) -> Result<()> {
        let parent_run_id = run.parent_run_id.map(|r| r.to_string());
        let ended_at = run.ended_at.map(|t| t.to_rfc3339());
        // Default `last_activity_at` to `started_at` so liveness queries always
        // see a heartbeat for newly-projected runs (§3.7.4).
        let last_activity_at = run.last_activity_at.unwrap_or(run.started_at).to_rfc3339();
        let unresponsive_at = run.unresponsive_at.map(|t| t.to_rfc3339());
        let stale_at = run.stale_at.map(|t| t.to_rfc3339());

        sqlx::query(
            "INSERT OR REPLACE INTO runs \
             (id, plan_id, agent_id, parent_run_id, started_at, ended_at, status, outcome, \
              last_activity_at, unresponsive_at, stale_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(run.id.to_string())
        .bind(run.plan_id.to_string())
        .bind(run.agent_id.to_string())
        .bind(parent_run_id)
        .bind(run.started_at.to_rfc3339())
        .bind(ended_at)
        .bind(run_status_str(run.status))
        .bind(&run.outcome)
        .bind(last_activity_at)
        .bind(unresponsive_at)
        .bind(stale_at)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(())
    }
}

// ── row mapping ───────────────────────────────────────────────────────────────

fn row_to_run(row: &sqlx::sqlite::SqliteRow) -> Result<Run> {
    let id: String = row
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let plan_id: String = row
        .try_get("plan_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let agent_id: String = row
        .try_get("agent_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let parent_run_id: Option<String> = row
        .try_get("parent_run_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let started_at_s: String = row
        .try_get("started_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let ended_at_s: Option<String> = row
        .try_get("ended_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let status_s: String = row
        .try_get("status")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let outcome: Option<String> = row
        .try_get("outcome")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let last_activity_at_s: Option<String> = row
        .try_get("last_activity_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let unresponsive_at_s: Option<String> = row
        .try_get("unresponsive_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let stale_at_s: Option<String> = row
        .try_get("stale_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    Ok(Run {
        id: id
            .parse::<RunId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        plan_id: plan_id
            .parse::<PlanId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        agent_id: agent_id
            .parse::<AgentId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        parent_run_id: parent_run_id
            .map(|s| {
                s.parse::<RunId>()
                    .map_err(|e| CoreError::serde(e.to_string()))
            })
            .transpose()?,
        started_at: parse_ts(&started_at_s)?,
        ended_at: ended_at_s.map(|s| parse_ts(&s)).transpose()?,
        status: parse_run_status(&status_s)?,
        outcome,
        last_activity_at: last_activity_at_s.map(|s| parse_ts(&s)).transpose()?,
        unresponsive_at: unresponsive_at_s.map(|s| parse_ts(&s)).transpose()?,
        stale_at: stale_at_s.map(|s| parse_ts(&s)).transpose()?,
    })
}

fn run_status_str(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Active => "active",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Aborted => "aborted",
    }
}

fn parse_run_status(s: &str) -> Result<RunStatus> {
    match s {
        "active" => Ok(RunStatus::Active),
        "completed" => Ok(RunStatus::Completed),
        "failed" => Ok(RunStatus::Failed),
        "aborted" => Ok(RunStatus::Aborted),
        other => Err(CoreError::serde(format!("unknown run status: {other}"))),
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_domain::Actor;
    use daruma_events::{Event, EventEnvelope};
    use daruma_shared::{time, AgentId, PlanId, RunId};

    async fn make_repo() -> (Db, RunRepo) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = RunRepo::new(db.pool().clone());
        (db, repo)
    }

    fn make_run(id: RunId, plan_id: PlanId) -> Run {
        Run {
            id,
            plan_id,
            agent_id: AgentId::new(),
            parent_run_id: None,
            started_at: time::now(),
            ended_at: None,
            status: RunStatus::Active,
            outcome: None,
            last_activity_at: None,
            unresponsive_at: None,
            stale_at: None,
        }
    }

    #[tokio::test]
    async fn run_start_and_get() {
        let (_db, repo) = make_repo().await;
        let run_id = RunId::new();
        let plan_id = PlanId::new();
        let run = make_run(run_id, plan_id);

        repo.start(&run).await.unwrap();

        let fetched = repo.get(run_id).await.unwrap().expect("run should exist");
        assert_eq!(fetched.id, run_id);
        assert_eq!(fetched.status, RunStatus::Active);
        assert!(fetched.ended_at.is_none());
    }

    #[tokio::test]
    async fn run_list_by_plan_and_active_filter() {
        let (_db, repo) = make_repo().await;
        let plan_id = PlanId::new();

        let r1 = make_run(RunId::new(), plan_id);
        let mut r2 = make_run(RunId::new(), plan_id);
        r2.status = RunStatus::Completed;
        r2.ended_at = Some(time::now());

        repo.start(&r1).await.unwrap();
        repo.start(&r2).await.unwrap();

        let all = repo.list_by_plan(plan_id).await.unwrap();
        assert_eq!(all.len(), 2);

        let active = repo.list_active_for_plan(plan_id).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, r1.id);
    }

    #[tokio::test]
    async fn run_complete_updates_status() {
        let (_db, repo) = make_repo().await;
        let run_id = RunId::new();
        let plan_id = PlanId::new();
        repo.start(&make_run(run_id, plan_id)).await.unwrap();

        repo.complete(run_id, time::now()).await.unwrap();

        let fetched = repo.get(run_id).await.unwrap().unwrap();
        assert_eq!(fetched.status, RunStatus::Completed);
        assert!(fetched.ended_at.is_some());
        assert_eq!(fetched.outcome.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn run_abort_updates_status_and_reason() {
        let (_db, repo) = make_repo().await;
        let run_id = RunId::new();
        let plan_id = PlanId::new();
        repo.start(&make_run(run_id, plan_id)).await.unwrap();

        repo.abort(run_id, "plan archived", time::now())
            .await
            .unwrap();

        let fetched = repo.get(run_id).await.unwrap().unwrap();
        assert_eq!(fetched.status, RunStatus::Aborted);
        assert_eq!(fetched.outcome.as_deref(), Some("plan archived"));
    }

    #[tokio::test]
    async fn run_apply_event_started() {
        let (_db, repo) = make_repo().await;
        let run = make_run(RunId::new(), PlanId::new());
        let run_id = run.id;

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::RunStarted { run },
        ))
        .await
        .unwrap();

        let fetched = repo.get(run_id).await.unwrap().expect("run should exist");
        assert_eq!(fetched.status, RunStatus::Active);
    }

    #[tokio::test]
    async fn run_apply_event_aborted() {
        let (_db, repo) = make_repo().await;
        let run = make_run(RunId::new(), PlanId::new());
        let run_id = run.id;
        repo.start(&run).await.unwrap();

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::RunAborted {
                run_id,
                reason: "stop requested".to_string(),
                at: time::now(),
            },
        ))
        .await
        .unwrap();

        let fetched = repo.get(run_id).await.unwrap().unwrap();
        assert_eq!(fetched.status, RunStatus::Aborted);
    }
}
