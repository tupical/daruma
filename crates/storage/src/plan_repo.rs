//! Plan projection repository — materialises plan/plan-task events into the
//! `plans` and `plan_tasks` SQLite tables.

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use taskagent_domain::{Plan, PlanProgress, PlanProgressSummary, PlanStatus, PlanTask};
use taskagent_events::{Event, EventEnvelope};
use taskagent_shared::{CoreError, PlanId, ProjectId, Result, TaskId, Timestamp};

/// Read/write access to the `plans` and `plan_tasks` projection tables.
pub struct PlanRepo {
    pub(crate) pool: SqlitePool,
}

impl PlanRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── plan queries ─────────────────────────────────────────────────────────

    pub async fn get(&self, id: PlanId) -> Result<Option<Plan>> {
        let row = sqlx::query(
            "SELECT id, project_id, parent_plan_id, title, description, goal, \
             success_criteria_json, status, owner_json, created_at, updated_at, archived_at, source_brief \
             FROM plans WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(row_to_plan).transpose()
    }

    pub async fn list_by_project(
        &self,
        project_id: ProjectId,
        status_filter: Option<PlanStatus>,
    ) -> Result<Vec<Plan>> {
        let rows = match status_filter {
            Some(s) => {
                sqlx::query(
                    "SELECT id, project_id, parent_plan_id, title, description, goal, \
                     success_criteria_json, status, owner_json, created_at, updated_at, archived_at, source_brief \
                     FROM plans WHERE project_id = ? AND status = ? ORDER BY created_at ASC",
                )
                .bind(project_id.to_string())
                .bind(plan_status_str(s))
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query(
                    "SELECT id, project_id, parent_plan_id, title, description, goal, \
                     success_criteria_json, status, owner_json, created_at, updated_at, archived_at, source_brief \
                     FROM plans WHERE project_id = ? ORDER BY created_at ASC",
                )
                .bind(project_id.to_string())
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_plan).collect()
    }

    pub async fn list_all(&self, status_filter: Option<PlanStatus>) -> Result<Vec<Plan>> {
        let rows = match status_filter {
            Some(s) => {
                sqlx::query(
                    "SELECT id, project_id, parent_plan_id, title, description, goal, \
                     success_criteria_json, status, owner_json, created_at, updated_at, archived_at, source_brief \
                     FROM plans WHERE status = ? ORDER BY created_at ASC",
                )
                .bind(plan_status_str(s))
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query(
                    "SELECT id, project_id, parent_plan_id, title, description, goal, \
                     success_criteria_json, status, owner_json, created_at, updated_at, archived_at, source_brief \
                     FROM plans ORDER BY created_at ASC",
                )
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_plan).collect()
    }

    pub async fn list_children(&self, parent_plan_id: PlanId) -> Result<Vec<Plan>> {
        let rows = sqlx::query(
            "SELECT id, project_id, parent_plan_id, title, description, goal, \
             success_criteria_json, status, owner_json, created_at, updated_at, archived_at, source_brief \
             FROM plans WHERE parent_plan_id = ? ORDER BY created_at ASC",
        )
        .bind(parent_plan_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_plan).collect()
    }

    /// Every plan that contains this task, ordered by membership position then
    /// plan creation time. Backed by `idx_plan_tasks_task` (migration 0008),
    /// so the join is bounded even at 10k+ tasks / plan_tasks rows.
    pub async fn list_plans_for_task(&self, task_id: TaskId) -> Result<Vec<Plan>> {
        let rows = sqlx::query(
            "SELECT p.id, p.project_id, p.parent_plan_id, p.title, p.description, p.goal, \
             p.success_criteria_json, p.status, p.owner_json, p.created_at, p.updated_at, p.archived_at, p.source_brief \
             FROM plans p \
             JOIN plan_tasks pt ON pt.plan_id = p.id \
             WHERE pt.task_id = ? \
             ORDER BY pt.position ASC, p.created_at ASC",
        )
        .bind(task_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_plan).collect()
    }

    // ── plan mutations ───────────────────────────────────────────────────────

    pub async fn insert(&self, plan: &Plan) -> Result<()> {
        self.upsert_plan(plan).await
    }

    pub async fn update_status(&self, plan_id: PlanId, status: PlanStatus) -> Result<()> {
        sqlx::query("UPDATE plans SET status = ?, updated_at = ? WHERE id = ?")
            .bind(plan_status_str(status))
            .bind(taskagent_shared::time::now().to_rfc3339())
            .bind(plan_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    pub async fn archive(&self, plan_id: PlanId, at: Timestamp) -> Result<()> {
        sqlx::query(
            "UPDATE plans SET status = 'abandoned', archived_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(at.to_rfc3339())
        .bind(at.to_rfc3339())
        .bind(plan_id.to_string())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    // ── plan_tasks queries ───────────────────────────────────────────────────

    pub async fn list_tasks_ordered(&self, plan_id: PlanId) -> Result<Vec<PlanTask>> {
        let rows = sqlx::query(
            "SELECT plan_id, task_id, position, depends_on_json \
             FROM plan_tasks WHERE plan_id = ? ORDER BY position ASC",
        )
        .bind(plan_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_plan_task).collect()
    }

    /// Compute derived progress for a plan — reads from `plan_tasks × tasks`
    /// and child `plans`.
    pub async fn get_progress(&self, plan_id: PlanId) -> Result<PlanProgress> {
        // task counts
        let row = sqlx::query(
            "SELECT COUNT(pt.task_id) AS tasks_total, \
             COALESCE(SUM(CASE WHEN t.status = 'done' THEN 1 ELSE 0 END), 0) AS tasks_done \
             FROM plan_tasks pt LEFT JOIN tasks t ON pt.task_id = t.id \
             WHERE pt.plan_id = ?",
        )
        .bind(plan_id.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        let tasks_total: i64 = row
            .try_get("tasks_total")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let tasks_done: i64 = row
            .try_get("tasks_done")
            .map_err(|e| CoreError::storage(e.to_string()))?;

        // sub-plan counts
        let row2 = sqlx::query(
            "SELECT COUNT(*) AS sub_plans_total, \
             COALESCE(SUM(CASE WHEN status = 'completed' THEN 1 ELSE 0 END), 0) AS sub_plans_done \
             FROM plans WHERE parent_plan_id = ?",
        )
        .bind(plan_id.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        let sub_plans_total: i64 = row2
            .try_get("sub_plans_total")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let sub_plans_done: i64 = row2
            .try_get("sub_plans_done")
            .map_err(|e| CoreError::storage(e.to_string()))?;

        let total = (tasks_total + sub_plans_total) as f32;
        let done = (tasks_done + sub_plans_done) as f32;
        let completion_pct = if total > 0.0 {
            (done / total * 100.0).min(100.0)
        } else {
            0.0
        };

        Ok(PlanProgress {
            tasks_total: tasks_total as u32,
            tasks_done: tasks_done as u32,
            sub_plans_total: sub_plans_total as u32,
            sub_plans_done: sub_plans_done as u32,
            completion_pct,
        })
    }

    /// Status breakdown for direct plan members — used by executor tooling.
    pub async fn get_progress_summary(&self, plan_id: PlanId) -> Result<PlanProgressSummary> {
        let row = sqlx::query(
            "SELECT COUNT(pt.task_id) AS total, \
             COALESCE(SUM(CASE WHEN t.status = 'done' THEN 1 ELSE 0 END), 0) AS done, \
             COALESCE(SUM(CASE WHEN t.status = 'in_progress' THEN 1 ELSE 0 END), 0) AS in_progress, \
             COALESCE(SUM(CASE WHEN t.status IN ('inbox', 'todo') THEN 1 ELSE 0 END), 0) AS todo \
             FROM plan_tasks pt LEFT JOIN tasks t ON pt.task_id = t.id \
             WHERE pt.plan_id = ?",
        )
        .bind(plan_id.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        let total: i64 = row
            .try_get("total")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let done: i64 = row
            .try_get("done")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let in_progress: i64 = row
            .try_get("in_progress")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let todo: i64 = row
            .try_get("todo")
            .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(PlanProgressSummary {
            total: total as u32,
            done: done as u32,
            in_progress: in_progress as u32,
            todo: todo as u32,
            next_ready: None,
        })
    }

    // ── plan_tasks mutations ─────────────────────────────────────────────────

    pub async fn add_task(
        &self,
        plan_id: PlanId,
        task_id: TaskId,
        position: u32,
        depends_on: &[TaskId],
    ) -> Result<()> {
        let depends_on_json =
            serde_json::to_string(depends_on).map_err(|e| CoreError::serde(e.to_string()))?;
        sqlx::query(
            "INSERT OR REPLACE INTO plan_tasks (plan_id, task_id, position, depends_on_json) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(plan_id.to_string())
        .bind(task_id.to_string())
        .bind(position as i64)
        .bind(depends_on_json)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    pub async fn remove_task(&self, plan_id: PlanId, task_id: TaskId) -> Result<()> {
        sqlx::query("DELETE FROM plan_tasks WHERE plan_id = ? AND task_id = ?")
            .bind(plan_id.to_string())
            .bind(task_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Replace the full task order. Each task_id in `order` gets its position
    /// updated to its index; tasks not present are left unchanged.
    pub async fn reorder(&self, plan_id: PlanId, order: &[TaskId]) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        for (pos, task_id) in order.iter().enumerate() {
            sqlx::query("UPDATE plan_tasks SET position = ? WHERE plan_id = ? AND task_id = ?")
                .bind(pos as i64)
                .bind(plan_id.to_string())
                .bind(task_id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    // ── event application ────────────────────────────────────────────────────

    pub async fn apply_event(&self, envelope: &EventEnvelope) -> Result<()> {
        let occurred_at = envelope.occurred_at;

        match &envelope.payload {
            Event::PlanCreated { plan } => {
                self.upsert_plan(plan).await?;
            }

            Event::PlanUpdated { plan_id, patch } => {
                if let Some(mut plan) = self.get(*plan_id).await? {
                    patch.clone().apply(&mut plan);
                    self.upsert_plan(&plan).await?;
                }
            }

            Event::PlanStatusChanged { plan_id, to, .. } => {
                sqlx::query("UPDATE plans SET status = ?, updated_at = ? WHERE id = ?")
                    .bind(plan_status_str(*to))
                    .bind(occurred_at.to_rfc3339())
                    .bind(plan_id.to_string())
                    .execute(&self.pool)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }

            Event::PlanGoalChanged { plan_id, to, .. } => {
                sqlx::query("UPDATE plans SET goal = ?, updated_at = ? WHERE id = ?")
                    .bind(to)
                    .bind(occurred_at.to_rfc3339())
                    .bind(plan_id.to_string())
                    .execute(&self.pool)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }

            Event::PlanTaskAdded {
                plan_id,
                task_id,
                position,
                depends_on,
            } => {
                self.add_task(*plan_id, *task_id, *position, depends_on)
                    .await?;
            }

            Event::PlanTaskRemoved { plan_id, task_id } => {
                self.remove_task(*plan_id, *task_id).await?;
            }

            Event::PlanReordered { plan_id, order } => {
                self.reorder(*plan_id, order).await?;
            }

            Event::PlanArchived { plan_id, at } => {
                self.archive(*plan_id, *at).await?;
            }

            _ => {}
        }

        Ok(())
    }

    // ── private helpers ──────────────────────────────────────────────────────

    async fn upsert_plan(&self, plan: &Plan) -> Result<()> {
        let owner_json =
            serde_json::to_string(&plan.owner).map_err(|e| CoreError::serde(e.to_string()))?;
        let sc_json = serde_json::to_string(&plan.success_criteria)
            .map_err(|e| CoreError::serde(e.to_string()))?;
        let parent_plan_id = plan.parent_plan_id.map(|p| p.to_string());
        let archived_at = plan.archived_at.map(|t| t.to_rfc3339());

        sqlx::query(
            "INSERT OR REPLACE INTO plans \
             (id, project_id, parent_plan_id, title, description, goal, \
              success_criteria_json, status, owner_json, created_at, updated_at, archived_at, source_brief) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(plan.id.to_string())
        .bind(plan.project_id.to_string())
        .bind(parent_plan_id)
        .bind(&plan.title)
        .bind(&plan.description)
        .bind(&plan.goal)
        .bind(sc_json)
        .bind(plan_status_str(plan.status))
        .bind(owner_json)
        .bind(plan.created_at.to_rfc3339())
        .bind(plan.updated_at.to_rfc3339())
        .bind(archived_at)
        .bind(plan.source_brief.clone())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(())
    }
}

// ── row mapping ───────────────────────────────────────────────────────────────

fn row_to_plan(row: &sqlx::sqlite::SqliteRow) -> Result<Plan> {
    let id: String = row
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let project_id: String = row
        .try_get("project_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let parent_plan_id: Option<String> = row
        .try_get("parent_plan_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let title: String = row
        .try_get("title")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let description: String = row
        .try_get("description")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let goal: String = row
        .try_get("goal")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let sc_json: String = row
        .try_get("success_criteria_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let status_s: String = row
        .try_get("status")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let owner_json: String = row
        .try_get("owner_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let created_at_s: String = row
        .try_get("created_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let updated_at_s: String = row
        .try_get("updated_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let archived_at_s: Option<String> = row
        .try_get("archived_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let source_brief: Option<String> = row
        .try_get("source_brief")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    Ok(Plan {
        id: id
            .parse::<PlanId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        project_id: project_id
            .parse::<ProjectId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        parent_plan_id: parent_plan_id
            .map(|s| {
                s.parse::<PlanId>()
                    .map_err(|e| CoreError::serde(e.to_string()))
            })
            .transpose()?,
        title,
        description,
        goal,
        success_criteria: serde_json::from_str(&sc_json)
            .map_err(|e| CoreError::serde(e.to_string()))?,
        status: parse_plan_status(&status_s)?,
        owner: serde_json::from_str(&owner_json).map_err(|e| CoreError::serde(e.to_string()))?,
        created_at: parse_ts(&created_at_s)?,
        updated_at: parse_ts(&updated_at_s)?,
        archived_at: archived_at_s.map(|s| parse_ts(&s)).transpose()?,
        source_brief,
    })
}

fn row_to_plan_task(row: &sqlx::sqlite::SqliteRow) -> Result<PlanTask> {
    let plan_id: String = row
        .try_get("plan_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let task_id: String = row
        .try_get("task_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let position: i64 = row
        .try_get("position")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let depends_on_json: String = row
        .try_get("depends_on_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    let depends_on: Vec<TaskId> =
        serde_json::from_str(&depends_on_json).map_err(|e| CoreError::serde(e.to_string()))?;

    Ok(PlanTask {
        plan_id: plan_id
            .parse::<PlanId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        task_id: task_id
            .parse::<TaskId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        position: position as u32,
        depends_on,
    })
}

fn plan_status_str(s: PlanStatus) -> &'static str {
    match s {
        PlanStatus::Draft => "draft",
        PlanStatus::Active => "active",
        PlanStatus::Completed => "completed",
        PlanStatus::Abandoned => "abandoned",
    }
}

fn parse_plan_status(s: &str) -> Result<PlanStatus> {
    match s {
        "draft" => Ok(PlanStatus::Draft),
        "active" => Ok(PlanStatus::Active),
        "completed" => Ok(PlanStatus::Completed),
        "abandoned" => Ok(PlanStatus::Abandoned),
        other => Err(CoreError::serde(format!("unknown plan status: {other}"))),
    }
}

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
    use taskagent_domain::Actor;
    use taskagent_events::{Event, EventEnvelope};
    use taskagent_shared::{time, PlanId, ProjectId, TaskId};

    async fn make_repo() -> (Db, PlanRepo) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = PlanRepo::new(db.pool().clone());
        (db, repo)
    }

    fn make_plan(id: PlanId, project_id: ProjectId) -> Plan {
        let now = time::now();
        Plan {
            id,
            project_id,
            parent_plan_id: None,
            title: "Test plan".to_string(),
            description: "desc".to_string(),
            goal: "goal".to_string(),
            success_criteria: vec!["c1".to_string()],
            status: PlanStatus::Draft,
            owner: Actor::user(),
            created_at: now,
            updated_at: now,
            archived_at: None,
            source_brief: None,
        }
    }

    #[tokio::test]
    async fn plan_insert_and_get() {
        let (_db, repo) = make_repo().await;
        let project_id = ProjectId::new();
        let plan = make_plan(PlanId::new(), project_id);

        repo.insert(&plan).await.unwrap();

        let fetched = repo.get(plan.id).await.unwrap().expect("plan should exist");
        assert_eq!(fetched.id, plan.id);
        assert_eq!(fetched.title, "Test plan");
        assert_eq!(fetched.status, PlanStatus::Draft);
    }

    #[tokio::test]
    async fn plan_list_by_project_with_status_filter() {
        let (_db, repo) = make_repo().await;
        let project_id = ProjectId::new();

        let mut p1 = make_plan(PlanId::new(), project_id);
        p1.status = PlanStatus::Active;
        let p2 = make_plan(PlanId::new(), project_id);

        repo.insert(&p1).await.unwrap();
        repo.insert(&p2).await.unwrap();

        let active = repo
            .list_by_project(project_id, Some(PlanStatus::Active))
            .await
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, p1.id);

        let all = repo.list_by_project(project_id, None).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn plan_list_children() {
        let (_db, repo) = make_repo().await;
        let project_id = ProjectId::new();
        let parent = make_plan(PlanId::new(), project_id);
        repo.insert(&parent).await.unwrap();

        let mut child = make_plan(PlanId::new(), project_id);
        child.parent_plan_id = Some(parent.id);
        repo.insert(&child).await.unwrap();

        let children = repo.list_children(parent.id).await.unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, child.id);
    }

    #[tokio::test]
    async fn plan_task_add_remove_reorder() {
        let (_db, repo) = make_repo().await;
        let plan_id = PlanId::new();
        let project_id = ProjectId::new();
        repo.insert(&make_plan(plan_id, project_id)).await.unwrap();

        let t1 = TaskId::new();
        let t2 = TaskId::new();
        let t3 = TaskId::new();

        repo.add_task(plan_id, t1, 0, &[]).await.unwrap();
        repo.add_task(plan_id, t2, 1, &[]).await.unwrap();
        repo.add_task(plan_id, t3, 2, &[]).await.unwrap();

        let tasks = repo.list_tasks_ordered(plan_id).await.unwrap();
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].task_id, t1);

        // reorder: put t3 first
        repo.reorder(plan_id, &[t3, t1, t2]).await.unwrap();
        let reordered = repo.list_tasks_ordered(plan_id).await.unwrap();
        assert_eq!(reordered[0].task_id, t3);
        assert_eq!(reordered[1].task_id, t1);

        // remove t2
        repo.remove_task(plan_id, t2).await.unwrap();
        let after_remove = repo.list_tasks_ordered(plan_id).await.unwrap();
        assert_eq!(after_remove.len(), 2);
    }

    #[tokio::test]
    async fn plan_progress_computed() {
        let (_db, repo) = make_repo().await;
        let plan_id = PlanId::new();
        let project_id = ProjectId::new();
        repo.insert(&make_plan(plan_id, project_id)).await.unwrap();

        // no tasks yet
        let p = repo.get_progress(plan_id).await.unwrap();
        assert_eq!(p.tasks_total, 0);
        assert!((p.completion_pct - 0.0).abs() < f32::EPSILON);

        // add tasks (can't mark done without task table rows, but count should work)
        let t1 = TaskId::new();
        repo.add_task(plan_id, t1, 0, &[]).await.unwrap();

        let p2 = repo.get_progress(plan_id).await.unwrap();
        assert_eq!(p2.tasks_total, 1);
        assert_eq!(p2.tasks_done, 0);
    }

    #[tokio::test]
    async fn plan_apply_event_plan_created() {
        let (_db, repo) = make_repo().await;
        let project_id = ProjectId::new();
        let plan = make_plan(PlanId::new(), project_id);
        let plan_id = plan.id;

        let env = EventEnvelope::new(Actor::user(), Event::PlanCreated { plan });
        repo.apply_event(&env).await.unwrap();

        let fetched = repo.get(plan_id).await.unwrap().expect("plan should exist");
        assert_eq!(fetched.id, plan_id);
    }

    #[tokio::test]
    async fn plan_apply_event_status_changed() {
        let (_db, repo) = make_repo().await;
        let project_id = ProjectId::new();
        let plan = make_plan(PlanId::new(), project_id);
        let plan_id = plan.id;
        repo.insert(&plan).await.unwrap();

        let env = EventEnvelope::new(
            Actor::user(),
            Event::PlanStatusChanged {
                plan_id,
                from: PlanStatus::Draft,
                to: PlanStatus::Active,
            },
        );
        repo.apply_event(&env).await.unwrap();

        let fetched = repo.get(plan_id).await.unwrap().unwrap();
        assert_eq!(fetched.status, PlanStatus::Active);
    }

    #[tokio::test]
    async fn plan_apply_event_task_added_and_archived() {
        let (_db, repo) = make_repo().await;
        let project_id = ProjectId::new();
        let plan = make_plan(PlanId::new(), project_id);
        let plan_id = plan.id;
        repo.insert(&plan).await.unwrap();

        let task_id = TaskId::new();
        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::PlanTaskAdded {
                plan_id,
                task_id,
                position: 0,
                depends_on: vec![],
            },
        ))
        .await
        .unwrap();

        let tasks = repo.list_tasks_ordered(plan_id).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, task_id);

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::PlanArchived {
                plan_id,
                at: time::now(),
            },
        ))
        .await
        .unwrap();

        let fetched = repo.get(plan_id).await.unwrap().unwrap();
        assert!(fetched.archived_at.is_some());
    }
}
