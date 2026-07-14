//! WorkUnit repository — projection over `work_units` (migration 0035)
//! plus the atomic claim CAS behind `work_unit_drain_next`.

use crate::parse_ts;
use chrono::{Duration, Utc};
use daruma_domain::{WorkUnit, WorkUnitStatus};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{AgentId, CoreError, ProjectId, Result, TaskId, Timestamp, WorkUnitId};
use sqlx::{Row, SqlitePool};

pub struct WorkUnitRepo {
    pool: SqlitePool,
}

impl WorkUnitRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn get(&self, id: WorkUnitId) -> Result<Option<WorkUnit>> {
        let row = sqlx::query(&select_sql("WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        row.as_ref().map(row_to_unit).transpose()
    }

    /// Units under a task, creation order. Terminal units included so the
    /// caller can render full decomposition state.
    pub async fn list_by_task(&self, task_id: TaskId) -> Result<Vec<WorkUnit>> {
        let rows = sqlx::query(&select_sql("WHERE task_id = ? ORDER BY created_at"))
            .bind(task_id.to_string())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.iter().map(row_to_unit).collect()
    }

    /// Units across a whole project, creation order. Joins `tasks` since
    /// `work_units` carries only `task_id` (no `project_id` column). Terminal
    /// units are included unless the caller narrows with `status`. This is the
    /// "Agent Operations" read: the project-wide work-unit queue.
    pub async fn list_by_project(
        &self,
        project_id: ProjectId,
        status: Option<WorkUnitStatus>,
    ) -> Result<Vec<WorkUnit>> {
        // Columns are qualified with `work_units.` because `tasks` shares
        // several bare names (id, status, priority, created_at, …).
        let base = "SELECT work_units.id, work_units.task_id, work_units.stage_plan_id, \
             work_units.title, work_units.description, work_units.status, work_units.priority, \
             work_units.capability_tags_json, work_units.owner_agent_id, \
             work_units.claim_expires_at, work_units.artifact_refs_json, \
             work_units.acceptance_json, work_units.created_at, work_units.updated_at \
             FROM work_units JOIN tasks ON work_units.task_id = tasks.id \
             WHERE tasks.project_id = ?";
        let rows = match status {
            Some(s) => {
                sqlx::query(&format!(
                    "{base} AND work_units.status = ? ORDER BY work_units.created_at"
                ))
                .bind(project_id.to_string())
                .bind(s.as_str())
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query(&format!("{base} ORDER BY work_units.created_at"))
                    .bind(project_id.to_string())
                    .fetch_all(&self.pool)
                    .await
            }
        }
        .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.iter().map(row_to_unit).collect()
    }

    /// Atomically claim the next dispatchable unit under `task_id` for
    /// `agent_id`: status `todo`/`ready` and no live claim by another
    /// agent. Single-statement compare-and-set — concurrent callers each
    /// get a *distinct* unit. Returns the claimed unit, or `None` when
    /// nothing is dispatchable.
    pub async fn try_claim_next(
        &self,
        task_id: TaskId,
        agent_id: AgentId,
        ttl: Duration,
    ) -> Result<Option<WorkUnit>> {
        let now = Utc::now();
        let now_s = now.to_rfc3339();
        let expires = (now + ttl).to_rfc3339();

        // The inner SELECT picks the oldest dispatchable unit; the UPDATE
        // re-checks the dispatchability predicate so a concurrent winner
        // makes this a 0-row no-op and the loop in the route retries.
        //
        // Handoff gate (P5): a unit with a non-accepted inbound handoff is
        // not ready — the consumer hasn't received what it needs yet. The
        // predicate matches `HandoffStatus::blocks_dispatch` (everything
        // except `accepted` blocks).
        //
        // Capability preference (P6): among dispatchable units, the ones
        // whose tags match the claiming agent's capability profiles come
        // first (best score wins, creation order breaks ties). Strictly a
        // preference — a zero-fit unit is still claimable. Profiles older
        // than 2 × their half-life stop contributing unless user-set
        // (ponytail: hard staleness cutoff instead of exponential decay;
        // upgrade if ordering quality warrants it).
        let updated = sqlx::query(
            "UPDATE work_units SET \
                owner_agent_id = ?, claim_expires_at = ?, status = 'in_progress', updated_at = ? \
             WHERE id = (\
                 SELECT id FROM work_units w \
                 WHERE task_id = ? AND status IN ('todo','ready') \
                   AND (owner_agent_id IS NULL OR owner_agent_id = ? \
                        OR claim_expires_at IS NULL OR claim_expires_at < ?) \
                   AND NOT EXISTS (\
                       SELECT 1 FROM handoff_contracts h \
                       WHERE h.to_work_unit_id = w.id AND h.status != 'accepted'\
                   ) \
                 ORDER BY COALESCE((\
                     SELECT MAX(p.score) FROM json_each(w.capability_tags_json) t \
                     JOIN agent_capability_profiles p \
                       ON p.agent_id = ? AND p.capability = t.value \
                     WHERE p.source = 'user_set' \
                        OR julianday(?) - julianday(p.last_observed_at) \
                           <= 2 * p.decay_half_life_days\
                 ), 0) DESC, created_at LIMIT 1\
             ) \
             AND status IN ('todo','ready') \
             AND (owner_agent_id IS NULL OR owner_agent_id = ? \
                  OR claim_expires_at IS NULL OR claim_expires_at < ?) \
             RETURNING id",
        )
        .bind(agent_id.to_string())
        .bind(&expires)
        .bind(&now_s)
        .bind(task_id.to_string())
        .bind(agent_id.to_string())
        .bind(&now_s)
        .bind(agent_id.to_string())
        .bind(&now_s)
        .bind(agent_id.to_string())
        .bind(&now_s)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        match updated {
            None => Ok(None),
            Some(row) => {
                let id: String = row
                    .try_get("id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let id = id
                    .parse::<WorkUnitId>()
                    .map_err(|e| CoreError::serde(e.to_string()))?;
                self.get(id).await
            }
        }
    }

    /// Revert a claim taken by [`try_claim_next`] (e.g. when the unit's
    /// declared resource leases could not be acquired). Restores `ready`.
    pub async fn revert_claim(&self, id: WorkUnitId, agent_id: AgentId) -> Result<()> {
        sqlx::query(
            "UPDATE work_units SET owner_agent_id = NULL, claim_expires_at = NULL, \
             status = 'ready', updated_at = ? \
             WHERE id = ? AND owner_agent_id = ?",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(id.to_string())
        .bind(agent_id.to_string())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    pub async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        let at = env.occurred_at;
        match &env.payload {
            Event::WorkUnitCreated { work_unit } => self.upsert(work_unit).await,
            Event::WorkUnitClaimed {
                work_unit_id,
                agent_id,
                expires_at,
            } => {
                self.update_fields(
                    *work_unit_id,
                    at,
                    "owner_agent_id = ?, claim_expires_at = ?, status = 'in_progress'",
                    vec![agent_id.to_string(), expires_at.to_rfc3339()],
                )
                .await
            }
            Event::WorkUnitStarted { work_unit_id, .. } => {
                self.update_fields(*work_unit_id, at, "status = 'in_progress'", vec![])
                    .await
            }
            Event::WorkUnitBlocked { work_unit_id, .. } => {
                self.update_fields(*work_unit_id, at, "status = 'blocked'", vec![])
                    .await
            }
            Event::WorkUnitCompleted {
                work_unit_id,
                completed_by,
                elapsed_ms,
                ..
            } => {
                // Terminal fact: stamp the mining columns (holder + cycle
                // time) alongside the status transition. `update_fields` only
                // binds strings, so `elapsed_ms` (INTEGER) goes through a
                // dedicated statement.
                sqlx::query(
                    "UPDATE work_units SET status = 'done', owner_agent_id = NULL, \
                     claim_expires_at = NULL, completed_by = ?, elapsed_ms = ?, updated_at = ? \
                     WHERE id = ?",
                )
                .bind(completed_by.as_ref().map(|a| a.to_string()))
                .bind(*elapsed_ms)
                .bind(at.to_rfc3339())
                .bind(work_unit_id.to_string())
                .execute(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(())
            }
            Event::WorkUnitReleased { work_unit_id, .. } => {
                self.update_fields(
                    *work_unit_id,
                    at,
                    "owner_agent_id = NULL, claim_expires_at = NULL, \
                     status = CASE WHEN status = 'in_progress' THEN 'ready' ELSE status END",
                    vec![],
                )
                .await
            }
            _ => Ok(()),
        }
    }

    async fn update_fields(
        &self,
        id: WorkUnitId,
        at: Timestamp,
        set_clause: &str,
        binds: Vec<String>,
    ) -> Result<()> {
        let sql = format!("UPDATE work_units SET {set_clause}, updated_at = ? WHERE id = ?");
        let mut q = sqlx::query(&sql);
        for b in &binds {
            q = q.bind(b);
        }
        q.bind(at.to_rfc3339())
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    async fn upsert(&self, wu: &WorkUnit) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO work_units \
             (id, task_id, stage_plan_id, title, description, status, priority, \
              capability_tags_json, owner_agent_id, claim_expires_at, \
              artifact_refs_json, acceptance_json, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(wu.id.to_string())
        .bind(wu.task_id.to_string())
        .bind(wu.stage_plan_id.map(|p| p.to_string()))
        .bind(&wu.title)
        .bind(&wu.description)
        .bind(wu.status.as_str())
        .bind(wu.priority.as_str())
        .bind(json_vec(&wu.capability_tags)?)
        .bind(wu.owner_agent_id.map(|a| a.to_string()))
        .bind(wu.claim_expires_at.map(|t| t.to_rfc3339()))
        .bind(json_vec(&wu.artifact_refs)?)
        .bind(json_vec(&wu.acceptance)?)
        .bind(wu.created_at.to_rfc3339())
        .bind(wu.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }
}

fn json_vec(v: &[String]) -> Result<String> {
    serde_json::to_string(v).map_err(|e| CoreError::serde(e.to_string()))
}

fn select_sql(filter: &str) -> String {
    format!(
        "SELECT id, task_id, stage_plan_id, title, description, status, priority, \
         capability_tags_json, owner_agent_id, claim_expires_at, artifact_refs_json, \
         acceptance_json, created_at, updated_at FROM work_units {filter}"
    )
}

fn row_to_unit(r: &sqlx::sqlite::SqliteRow) -> Result<WorkUnit> {
    fn col<T>(v: std::result::Result<T, sqlx::Error>) -> Result<T> {
        v.map_err(|e| CoreError::storage(e.to_string()))
    }
    fn parse_vec(s: &str) -> Result<Vec<String>> {
        serde_json::from_str(s).map_err(|e| CoreError::serde(e.to_string()))
    }

    let id: String = col(r.try_get("id"))?;
    let task_id: String = col(r.try_get("task_id"))?;
    let stage: Option<String> = col(r.try_get("stage_plan_id"))?;
    let status_s: String = col(r.try_get("status"))?;
    let priority_s: String = col(r.try_get("priority"))?;
    let owner: Option<String> = col(r.try_get("owner_agent_id"))?;
    let claim: Option<String> = col(r.try_get("claim_expires_at"))?;
    let tags: String = col(r.try_get("capability_tags_json"))?;
    let refs: String = col(r.try_get("artifact_refs_json"))?;
    let acceptance: String = col(r.try_get("acceptance_json"))?;
    let created: String = col(r.try_get("created_at"))?;
    let updated: String = col(r.try_get("updated_at"))?;

    Ok(WorkUnit {
        id: id
            .parse()
            .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?,
        task_id: task_id
            .parse()
            .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?,
        stage_plan_id: stage
            .map(|s| s.parse())
            .transpose()
            .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?,
        title: col(r.try_get("title"))?,
        description: col(r.try_get("description"))?,
        status: WorkUnitStatus::parse(&status_s)
            .ok_or_else(|| CoreError::serde(format!("bad work unit status {status_s}")))?,
        priority: serde_json::from_value(serde_json::Value::String(priority_s.clone()))
            .map_err(|_| CoreError::serde(format!("bad priority {priority_s}")))?,
        capability_tags: parse_vec(&tags)?,
        owner_agent_id: owner
            .map(|s| s.parse())
            .transpose()
            .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?,
        claim_expires_at: claim.as_deref().map(parse_ts).transpose()?,
        artifact_refs: parse_vec(&refs)?,
        acceptance: parse_vec(&acceptance)?,
        created_at: parse_ts(&created)?,
        updated_at: parse_ts(&updated)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_domain::Actor;

    async fn repo() -> (Db, WorkUnitRepo) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let r = WorkUnitRepo::new(db.pool().clone());
        (db, r)
    }

    async fn seed(r: &WorkUnitRepo, task: TaskId, title: &str) -> WorkUnit {
        let wu = {
            let mut wu = WorkUnit::sample(task);
            wu.title = title.into();
            wu
        };
        let env = EventEnvelope::new(
            Actor::user(),
            Event::WorkUnitCreated {
                work_unit: wu.clone(),
            },
        );
        r.apply_event(&env).await.unwrap();
        wu
    }

    async fn seed_task(db: &Db, task: TaskId, project: ProjectId) {
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
    }

    #[tokio::test]
    async fn list_by_project_joins_tasks_and_filters_status() {
        let (db, r) = repo().await;
        let project = ProjectId::new();
        let other_project = ProjectId::new();
        let task = TaskId::new();
        let other_task = TaskId::new();
        seed_task(&db, task, project).await;
        seed_task(&db, other_task, other_project).await;

        seed(&r, task, "a").await;
        seed(&r, task, "b").await;
        seed(&r, other_task, "elsewhere").await;

        // Project scope excludes the unit under the other project.
        let all = r.list_by_project(project, None).await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|u| u.task_id == task));

        // Status filter narrows to matching units.
        let agent = AgentId::new();
        r.try_claim_next(task, agent, Duration::seconds(60))
            .await
            .unwrap()
            .unwrap();
        let in_progress = r
            .list_by_project(project, Some(WorkUnitStatus::InProgress))
            .await
            .unwrap();
        assert_eq!(in_progress.len(), 1);
        assert_eq!(in_progress[0].status, WorkUnitStatus::InProgress);
        let todo = r
            .list_by_project(project, Some(WorkUnitStatus::Todo))
            .await
            .unwrap();
        assert_eq!(todo.len(), 1);

        // Empty for a project with no work units.
        assert!(r
            .list_by_project(ProjectId::new(), None)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn concurrent_claims_get_distinct_units() {
        let (_db, r) = repo().await;
        let task = TaskId::new();
        seed(&r, task, "a").await;
        seed(&r, task, "b").await;

        let (a1, a2) = (AgentId::new(), AgentId::new());
        let u1 = r
            .try_claim_next(task, a1, Duration::seconds(60))
            .await
            .unwrap()
            .expect("first unit");
        let u2 = r
            .try_claim_next(task, a2, Duration::seconds(60))
            .await
            .unwrap()
            .expect("second unit");
        assert_ne!(u1.id, u2.id, "no duplicate dispatch");
        assert_eq!(u1.status, WorkUnitStatus::InProgress);

        // Pool drained.
        assert!(r
            .try_claim_next(task, AgentId::new(), Duration::seconds(60))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn revert_claim_returns_unit_to_pool() {
        let (_db, r) = repo().await;
        let task = TaskId::new();
        seed(&r, task, "a").await;
        let agent = AgentId::new();
        let u = r
            .try_claim_next(task, agent, Duration::seconds(60))
            .await
            .unwrap()
            .unwrap();
        r.revert_claim(u.id, agent).await.unwrap();
        let again = r
            .try_claim_next(task, AgentId::new(), Duration::seconds(60))
            .await
            .unwrap()
            .expect("unit is dispatchable again");
        assert_eq!(again.id, u.id);
    }

    #[tokio::test]
    async fn completed_units_leave_the_pool_and_release_restores_ready() {
        let (_db, r) = repo().await;
        let task = TaskId::new();
        let wu = seed(&r, task, "a").await;
        let agent = AgentId::new();
        r.try_claim_next(task, agent, Duration::seconds(60))
            .await
            .unwrap()
            .unwrap();

        let env = EventEnvelope::new(
            Actor::user(),
            Event::WorkUnitReleased {
                work_unit_id: wu.id,
                at: Utc::now(),
            },
        );
        r.apply_event(&env).await.unwrap();
        assert_eq!(
            r.get(wu.id).await.unwrap().unwrap().status,
            WorkUnitStatus::Ready
        );

        let env = EventEnvelope::new(
            Actor::user(),
            Event::WorkUnitCompleted {
                work_unit_id: wu.id,
                outcome: "ok".into(),
                produced_artifacts: vec!["artifact://api/users".into()],
                next_suggested_units: vec![],
                completed_by: None,
                elapsed_ms: None,
                at: Utc::now(),
            },
        );
        r.apply_event(&env).await.unwrap();
        let done = r.get(wu.id).await.unwrap().unwrap();
        assert_eq!(done.status, WorkUnitStatus::Done);
        assert!(done.owner_agent_id.is_none());
        assert!(r
            .try_claim_next(task, agent, Duration::seconds(60))
            .await
            .unwrap()
            .is_none());
    }

    /// P6 mining facts: `WorkUnitCompleted` stamps the holder and cycle time
    /// into the projection's write-through columns.
    #[tokio::test]
    async fn completion_persists_mining_facts() {
        let (_db, r) = repo().await;
        let task = TaskId::new();
        let wu = seed(&r, task, "a").await;
        let holder = AgentId::new();

        r.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::WorkUnitCompleted {
                work_unit_id: wu.id,
                outcome: "ok".into(),
                produced_artifacts: vec![],
                next_suggested_units: vec![],
                completed_by: Some(holder),
                elapsed_ms: Some(4200),
                at: Utc::now(),
            },
        ))
        .await
        .unwrap();

        let row = sqlx::query("SELECT completed_by, elapsed_ms FROM work_units WHERE id = ?")
            .bind(wu.id.to_string())
            .fetch_one(&r.pool)
            .await
            .unwrap();
        let completed_by: Option<String> = row.try_get("completed_by").unwrap();
        let elapsed_ms: Option<i64> = row.try_get("elapsed_ms").unwrap();
        assert_eq!(completed_by, Some(holder.to_string()));
        assert_eq!(elapsed_ms, Some(4200));
    }

    /// P5 handoff gate: a unit with a non-accepted inbound handoff is not
    /// dispatchable; accepting the handoff returns it to the pool.
    #[tokio::test]
    async fn non_accepted_inbound_handoff_blocks_drain() {
        let (db, r) = repo().await;
        let handoffs = crate::HandoffRepo::new(db.pool().clone());
        let task = TaskId::new();
        let producer = seed(&r, task, "producer").await;
        let consumer = seed(&r, task, "consumer").await;

        // Open handoff into `consumer`: only `producer` is dispatchable.
        let contract = daruma_domain::NewHandoffContract {
            from_work_unit_id: producer.id,
            to_work_unit_id: consumer.id,
            required_artifact_ids: vec![],
            required_state: None,
            checklist: vec![],
            owner_agent_id: None,
        }
        .into_contract(daruma_shared::HandoffId::new(), daruma_shared::time::now());
        let handoff_id = contract.id;
        handoffs
            .apply_event(&EventEnvelope::new(
                Actor::user(),
                Event::HandoffRequested { handoff: contract },
            ))
            .await
            .unwrap();

        let agent = AgentId::new();
        let ttl = chrono::Duration::seconds(300);
        let first = r.try_claim_next(task, agent, ttl).await.unwrap().unwrap();
        assert_eq!(
            first.id, producer.id,
            "consumer is gated, producer drains first"
        );
        assert!(
            r.try_claim_next(task, AgentId::new(), ttl)
                .await
                .unwrap()
                .is_none(),
            "gated consumer must not be dispatched"
        );

        // Rejection still blocks (DoD: not dispatched until accepted).
        handoffs
            .apply_event(&EventEnvelope::new(
                Actor::user(),
                Event::HandoffRejected {
                    handoff_id,
                    reason: "not ready".into(),
                    required_changes: vec![],
                    latency_ms: None,
                    at: daruma_shared::time::now(),
                },
            ))
            .await
            .unwrap();
        assert!(r
            .try_claim_next(task, AgentId::new(), ttl)
            .await
            .unwrap()
            .is_none());

        // Acceptance unblocks the consumer.
        handoffs
            .apply_event(&EventEnvelope::new(
                Actor::user(),
                Event::HandoffAccepted {
                    handoff_id,
                    by: None,
                    notes: None,
                    latency_ms: None,
                    at: daruma_shared::time::now(),
                },
            ))
            .await
            .unwrap();
        let next = r
            .try_claim_next(task, AgentId::new(), ttl)
            .await
            .unwrap()
            .expect("consumer dispatchable after acceptance");
        assert_eq!(next.id, consumer.id);
    }

    /// P6 capability preference: among dispatchable units the claiming
    /// agent's profile reorders the pool (preference), but a zero-fit
    /// agent still gets a unit (never a hard binding).
    #[tokio::test]
    async fn capability_profile_prefers_matching_unit_without_binding() {
        let (db, r) = repo().await;
        let profiles = crate::CapabilityProfileRepo::new(db.pool().clone());
        let task = TaskId::new();

        // Older unit: backend. Newer unit: frontend. FIFO would pick backend.
        let backend = {
            let mut wu = WorkUnit::sample(task);
            wu.title = "backend".into();
            wu.capability_tags = vec!["backend".into()];
            let env = EventEnvelope::new(
                Actor::user(),
                Event::WorkUnitCreated {
                    work_unit: wu.clone(),
                },
            );
            r.apply_event(&env).await.unwrap();
            wu
        };
        let frontend = {
            let mut wu = WorkUnit::sample(task);
            wu.title = "frontend".into();
            wu.capability_tags = vec!["frontend".into()];
            let env = EventEnvelope::new(
                Actor::user(),
                Event::WorkUnitCreated {
                    work_unit: wu.clone(),
                },
            );
            r.apply_event(&env).await.unwrap();
            wu
        };

        let ttl = chrono::Duration::seconds(300);
        let specialist = AgentId::new();
        profiles
            .upsert_user_set(
                specialist,
                "frontend",
                0.9,
                &daruma_shared::time::now().to_rfc3339(),
            )
            .await
            .unwrap();

        // The frontend specialist jumps the FIFO order.
        let got = r
            .try_claim_next(task, specialist, ttl)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.id, frontend.id, "profile fit beats creation order");

        // A profile-less agent still gets the remaining unit — preference,
        // not a binding.
        let generalist = AgentId::new();
        let got = r
            .try_claim_next(task, generalist, ttl)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.id, backend.id);
    }
}
