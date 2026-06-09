//! Task projection repository — materialises task-related events into the
//! `tasks` SQLite table.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use taskagent_domain::{Priority, Status, Task, TriageState};
use taskagent_events::{Event, EventEnvelope};
use taskagent_shared::{CoreError, EventId, ProjectId, Result, TaskId};

use crate::entity_version::{insert_entity_version, update_summary};

/// Read/write access to the `tasks` projection table.
pub struct TaskRepo {
    pub(crate) pool: SqlitePool,
}

impl TaskRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── queries ──────────────────────────────────────────────────────────────

    pub async fn list_all(&self) -> Result<Vec<Task>> {
        self.list_all_filtered(&[]).await
    }

    pub async fn list_all_filtered(&self, statuses: &[Status]) -> Result<Vec<Task>> {
        let (status_clause, status_binds) = status_clause(statuses, /*has_where=*/ false);
        let sql = format!(
            "SELECT id, project_id, title, description, status, priority, \
             due_at, created_at, updated_at, started_at, completed_at, \
             created_by_json, completed_by_json, updated_by_json, updated_event_id, \
             updated_event_seq, source_event_id, triage_state \
             FROM tasks{status_clause} ORDER BY created_at ASC"
        );
        let mut q = sqlx::query(&sql);
        for s in status_binds {
            q = q.bind(s);
        }
        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_task).collect()
    }

    pub async fn list_by_project(&self, project_id: Option<ProjectId>) -> Result<Vec<Task>> {
        self.list_by_project_filtered(project_id, &[]).await
    }

    pub async fn list_by_project_filtered(
        &self,
        project_id: Option<ProjectId>,
        statuses: &[Status],
    ) -> Result<Vec<Task>> {
        let (status_clause, status_binds) = status_clause(statuses, /*has_where=*/ true);
        let (where_clause, project_bind) = match project_id {
            Some(pid) => ("WHERE project_id = ?", Some(pid.to_string())),
            None => ("WHERE project_id IS NULL", None),
        };
        let sql = format!(
            "SELECT id, project_id, title, description, status, priority, \
             due_at, created_at, updated_at, started_at, completed_at, \
             created_by_json, completed_by_json, updated_by_json, updated_event_id, \
             updated_event_seq, source_event_id, triage_state \
             FROM tasks {where_clause}{status_clause} ORDER BY created_at ASC"
        );
        let mut q = sqlx::query(&sql);
        if let Some(pid) = project_bind {
            q = q.bind(pid);
        }
        for s in status_binds {
            q = q.bind(s);
        }
        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_task).collect()
    }

    pub async fn list_by_status(&self, status: Status) -> Result<Vec<Task>> {
        let rows = sqlx::query(
            "SELECT id, project_id, title, description, status, priority, \
             due_at, created_at, updated_at, started_at, completed_at, \
             created_by_json, completed_by_json, updated_by_json, updated_event_id, \
             updated_event_seq, source_event_id, triage_state \
             FROM tasks \
             WHERE status = ? ORDER BY created_at ASC",
        )
        .bind(status.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_task).collect()
    }

    pub async fn get(&self, id: TaskId) -> Result<Option<Task>> {
        let row = sqlx::query(
            "SELECT id, project_id, title, description, status, priority, \
             due_at, created_at, updated_at, started_at, completed_at, \
             created_by_json, completed_by_json, updated_by_json, updated_event_id, \
             updated_event_seq, source_event_id, triage_state \
             FROM tasks WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(row_to_task).transpose()
    }

    pub async fn list_triage_queue(&self, project_id: ProjectId) -> Result<Vec<Task>> {
        let rows = sqlx::query(
            "SELECT id, project_id, title, description, status, priority, \
             due_at, created_at, updated_at, started_at, completed_at, \
             created_by_json, completed_by_json, updated_by_json, updated_event_id, \
             updated_event_seq, source_event_id, triage_state \
             FROM tasks \
             WHERE project_id = ? AND triage_state = 'pending' \
             ORDER BY created_at ASC",
        )
        .bind(project_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.iter().map(row_to_task).collect()
    }

    pub async fn set_triage_state(
        &self,
        id: TaskId,
        state: Option<TriageState>,
    ) -> Result<Option<Task>> {
        let n = sqlx::query("UPDATE tasks SET triage_state = ?, updated_at = ? WHERE id = ?")
            .bind(state.map(TriageState::as_str))
            .bind(chrono::Utc::now().to_rfc3339())
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?
            .rows_affected();
        if n == 0 {
            return Ok(None);
        }
        self.get(id).await
    }

    pub async fn get_many(&self, ids: &[TaskId]) -> Result<Vec<Task>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat("?")
            .take(ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, project_id, title, description, status, priority, \
             due_at, created_at, updated_at, started_at, completed_at, \
             created_by_json, completed_by_json, updated_by_json, updated_event_id, \
             updated_event_seq, source_event_id, triage_state \
             FROM tasks WHERE id IN ({ph})",
            ph = placeholders
        );
        let mut q = sqlx::query(&sql);
        for id in ids {
            q = q.bind(id.to_string());
        }
        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.iter().map(row_to_task).collect()
    }

    // ── mutations ────────────────────────────────────────────────────────────

    /// Apply a single event envelope, updating the `tasks` projection.
    ///
    /// Non-task events are silently ignored.
    pub async fn apply_event(&self, envelope: &EventEnvelope) -> Result<()> {
        let occurred_at = envelope.occurred_at;

        match &envelope.payload {
            Event::TaskCreated { task: new_task } => {
                let mut tx = self.begin_tx().await?;
                let task = Task {
                    id: new_task.id.unwrap_or_default(),
                    project_id: new_task.project_id,
                    title: new_task.title.clone(),
                    description: new_task.description.clone().unwrap_or_default(),
                    status: new_task.status.unwrap_or_default(),
                    priority: new_task.priority.unwrap_or_default(),
                    triage_state: new_task.triage_state,
                    due_at: new_task.due_at,
                    created_at: occurred_at,
                    updated_at: occurred_at,
                    started_at: None,
                    completed_at: None,
                    created_by: Some(envelope.actor.clone()),
                    completed_by: None,
                    updated_by: Some(envelope.actor.clone()),
                    updated_event_id: Some(envelope.id),
                    updated_event_seq: Some(envelope.seq),
                    source_event_id: None,
                };
                let after = task_value(&task)?;
                self.upsert_task_tx(&mut tx, &task).await?;
                insert_task_version(&mut tx, envelope, task.id, None, Some(after)).await?;
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }

            Event::TaskDueElapsed {
                task_id,
                due_at,
                at,
            } => {
                sqlx::query(
                    "INSERT OR REPLACE INTO task_due_notifications \
                     (task_id, due_at, notified_at) VALUES (?, ?, ?)",
                )
                .bind(task_id.to_string())
                .bind(due_at.to_rfc3339())
                .bind(at.to_rfc3339())
                .execute(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
            }

            Event::TaskUpdated { task_id, patch } => {
                let mut tx = self.begin_tx().await?;
                if let Some(mut task) = get_task_tx(&mut tx, *task_id).await? {
                    let before = task_value(&task)?;
                    patch.clone().apply(&mut task);
                    task.updated_at = occurred_at;
                    stamp_last_change(&mut task, envelope);
                    let after = task_value(&task)?;
                    self.upsert_task_tx(&mut tx, &task).await?;
                    insert_task_version(&mut tx, envelope, *task_id, Some(before), Some(after))
                        .await?;
                }
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }

            Event::TaskStatusChanged { task_id, to, .. } => {
                let mut tx = self.begin_tx().await?;
                if let Some(mut task) = get_task_tx(&mut tx, *task_id).await? {
                    let before = task_value(&task)?;
                    task.status = *to;
                    task.updated_at = occurred_at;
                    if *to == Status::InProgress || *to == Status::Done {
                        task.started_at = task.started_at.or(Some(occurred_at));
                    }
                    if *to == Status::Done {
                        task.completed_by = Some(envelope.actor.clone());
                    }
                    stamp_last_change(&mut task, envelope);
                    let after = task_value(&task)?;
                    self.upsert_task_tx(&mut tx, &task).await?;
                    insert_task_version(&mut tx, envelope, *task_id, Some(before), Some(after))
                        .await?;
                }
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }

            Event::TaskPriorityChanged { task_id, to, .. } => {
                let mut tx = self.begin_tx().await?;
                if let Some(mut task) = get_task_tx(&mut tx, *task_id).await? {
                    let before = task_value(&task)?;
                    task.priority = *to;
                    task.updated_at = occurred_at;
                    stamp_last_change(&mut task, envelope);
                    let after = task_value(&task)?;
                    self.upsert_task_tx(&mut tx, &task).await?;
                    insert_task_version(&mut tx, envelope, *task_id, Some(before), Some(after))
                        .await?;
                }
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }

            Event::TaskCompleted {
                task_id,
                completed_at,
            } => {
                let mut tx = self.begin_tx().await?;
                if let Some(mut task) = get_task_tx(&mut tx, *task_id).await? {
                    let before = task_value(&task)?;
                    task.status = Status::Done;
                    task.updated_at = occurred_at;
                    task.completed_at = Some(*completed_at);
                    task.started_at = task.started_at.or(Some(*completed_at));
                    task.completed_by = Some(envelope.actor.clone());
                    stamp_last_change(&mut task, envelope);
                    let after = task_value(&task)?;
                    self.upsert_task_tx(&mut tx, &task).await?;
                    insert_task_version(&mut tx, envelope, *task_id, Some(before), Some(after))
                        .await?;
                }
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }

            Event::TaskDeleted { task_id } => {
                let mut tx = self.begin_tx().await?;
                let before = get_task_tx(&mut tx, *task_id)
                    .await?
                    .map(|task| task_value(&task))
                    .transpose()?;
                sqlx::query("DELETE FROM tasks WHERE id = ?")
                    .bind(task_id.to_string())
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                sqlx::query("DELETE FROM task_due_notifications WHERE task_id = ?")
                    .bind(task_id.to_string())
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                if let Some(before) = before {
                    insert_task_version(&mut tx, envelope, *task_id, Some(before), None).await?;
                }
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }

            Event::TaskSplitGenerated { subtasks, .. } => {
                let mut tx = self.begin_tx().await?;
                for new_task in subtasks {
                    let task = Task {
                        id: new_task.id.unwrap_or_default(),
                        project_id: new_task.project_id,
                        title: new_task.title.clone(),
                        description: new_task.description.clone().unwrap_or_default(),
                        status: new_task.status.unwrap_or_default(),
                        priority: new_task.priority.unwrap_or_default(),
                        triage_state: new_task.triage_state,
                        due_at: new_task.due_at,
                        created_at: occurred_at,
                        updated_at: occurred_at,
                        started_at: None,
                        completed_at: None,
                        created_by: Some(envelope.actor.clone()),
                        completed_by: None,
                        updated_by: Some(envelope.actor.clone()),
                        updated_event_id: Some(envelope.id),
                        updated_event_seq: Some(envelope.seq),
                        // §3.8.10: subtasks generated by a split point
                        // back to the originating event so provenance
                        // can be walked later.
                        source_event_id: Some(envelope.id),
                    };
                    let after = task_value(&task)?;
                    self.upsert_task_tx(&mut tx, &task).await?;
                    insert_task_version(&mut tx, envelope, task.id, None, Some(after)).await?;
                }
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }

            // Semantic events (W2.1) — update lifecycle timestamps in
            // addition to the mechanical `TaskStatusChanged` that precedes
            // them in the same batch.
            Event::TaskReopened { task_id, at, .. } => {
                let mut tx = self.begin_tx().await?;
                if let Some(mut task) = get_task_tx(&mut tx, *task_id).await? {
                    let before = task_value(&task)?;
                    task.completed_at = None;
                    task.updated_at = *at;
                    stamp_last_change(&mut task, envelope);
                    let after = task_value(&task)?;
                    self.upsert_task_tx(&mut tx, &task).await?;
                    insert_task_version(&mut tx, envelope, *task_id, Some(before), Some(after))
                        .await?;
                }
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }

            Event::TaskClosed { task_id, at, .. } => {
                let mut tx = self.begin_tx().await?;
                if let Some(mut task) = get_task_tx(&mut tx, *task_id).await? {
                    let before = task_value(&task)?;
                    task.completed_at = Some(*at);
                    task.updated_at = *at;
                    task.started_at = task.started_at.or(Some(*at));
                    stamp_last_change(&mut task, envelope);
                    let after = task_value(&task)?;
                    self.upsert_task_tx(&mut tx, &task).await?;
                    insert_task_version(&mut tx, envelope, *task_id, Some(before), Some(after))
                        .await?;
                }
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }

            // Non-task projection-affecting events are ignored by this repo.
            _ => {}
        }

        Ok(())
    }

    // ── private helpers ───────────────────────────────────────────────────────

    async fn begin_tx(&self) -> Result<Transaction<'_, Sqlite>> {
        self.pool
            .begin()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))
    }

    /// Tasks whose `due_at` has passed while still in a non-terminal
    /// status and that have not yet been notified for *this* deadline
    /// value (`task.due` webhook watchdog, capped at `limit`).
    pub async fn list_due_unnotified(
        &self,
        now: taskagent_shared::Timestamp,
        limit: u32,
    ) -> Result<Vec<(TaskId, taskagent_shared::Timestamp)>> {
        let rows = sqlx::query(
            "SELECT t.id, t.due_at FROM tasks t \
             WHERE t.due_at IS NOT NULL \
               AND t.due_at < ? \
               AND t.status IN ('inbox', 'todo', 'in_progress', 'in_review') \
               AND NOT EXISTS (\
                   SELECT 1 FROM task_due_notifications n \
                   WHERE n.task_id = t.id AND n.due_at = t.due_at\
               ) \
             ORDER BY t.due_at ASC LIMIT ?",
        )
        .bind(now.to_rfc3339())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let id_s: String = row
                .try_get("id")
                .map_err(|e| CoreError::storage(e.to_string()))?;
            let due_s: String = row
                .try_get("due_at")
                .map_err(|e| CoreError::storage(e.to_string()))?;
            let task_id = id_s
                .parse::<TaskId>()
                .map_err(|e| CoreError::serde(e.to_string()))?;
            out.push((task_id, parse_ts(&due_s)?));
        }
        Ok(out)
    }

    async fn upsert_task_tx(&self, tx: &mut Transaction<'_, Sqlite>, task: &Task) -> Result<()> {
        let project_id = task.project_id.map(|p| p.to_string());
        let due_at = task.due_at.map(|t| t.to_rfc3339());
        let started_at = task.started_at.map(|t| t.to_rfc3339());
        let completed_at = task.completed_at.map(|t| t.to_rfc3339());
        let created_by_json = task
            .created_by
            .as_ref()
            .map(|a| serde_json::to_string(a).map_err(|e| CoreError::serde(e.to_string())))
            .transpose()?;
        let completed_by_json = task
            .completed_by
            .as_ref()
            .map(|a| serde_json::to_string(a).map_err(|e| CoreError::serde(e.to_string())))
            .transpose()?;
        let updated_by_json = task
            .updated_by
            .as_ref()
            .map(|a| serde_json::to_string(a).map_err(|e| CoreError::serde(e.to_string())))
            .transpose()?;
        let updated_event_id = task.updated_event_id.map(|id| id.to_string());
        let updated_event_seq = task.updated_event_seq.map(|seq| seq as i64);

        let source_event_id = task.source_event_id.map(|id| id.to_string());

        sqlx::query(
            "INSERT OR REPLACE INTO tasks \
             (id, project_id, title, description, status, priority, due_at, \
              created_at, updated_at, started_at, completed_at, \
              created_by_json, completed_by_json, updated_by_json, updated_event_id, \
              updated_event_seq, source_event_id, triage_state) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(task.id.to_string())
        .bind(project_id)
        .bind(&task.title)
        .bind(&task.description)
        .bind(task.status.as_str())
        .bind(task.priority.as_str())
        .bind(due_at)
        .bind(task.created_at.to_rfc3339())
        .bind(task.updated_at.to_rfc3339())
        .bind(started_at)
        .bind(completed_at)
        .bind(created_by_json)
        .bind(completed_by_json)
        .bind(updated_by_json)
        .bind(updated_event_id)
        .bind(updated_event_seq)
        .bind(source_event_id)
        .bind(task.triage_state.map(TriageState::as_str))
        .execute(&mut **tx)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(())
    }
}

async fn get_task_tx(tx: &mut Transaction<'_, Sqlite>, id: TaskId) -> Result<Option<Task>> {
    let row = sqlx::query(
        "SELECT id, project_id, title, description, status, priority, \
         due_at, created_at, updated_at, started_at, completed_at, \
         created_by_json, completed_by_json, updated_by_json, updated_event_id, \
         updated_event_seq, source_event_id, triage_state \
         FROM tasks WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| CoreError::storage(e.to_string()))?;

    row.as_ref().map(row_to_task).transpose()
}

async fn insert_task_version(
    tx: &mut Transaction<'_, Sqlite>,
    envelope: &EventEnvelope,
    task_id: TaskId,
    before: Option<Value>,
    after: Option<Value>,
) -> Result<()> {
    let summary = update_summary("Task", before.as_ref(), after.as_ref());
    insert_entity_version(
        tx,
        "task",
        task_id.to_string(),
        before,
        after,
        envelope,
        summary,
    )
    .await
}

fn task_value(task: &Task) -> Result<Value> {
    serde_json::to_value(task).map_err(|e| CoreError::serde(e.to_string()))
}

// ── filter helpers ────────────────────────────────────────────────────────────

/// Build a `status IN (?, ?, …)` fragment plus the values to bind.
///
/// `has_where` selects between `" WHERE …"` and `" AND …"` so the caller
/// can chain the fragment onto either a bare `FROM tasks` or an existing
/// `WHERE …` clause. An empty `statuses` slice yields no fragment and no
/// binds — callers must handle that to skip the filter entirely.
fn status_clause(statuses: &[Status], has_where: bool) -> (String, Vec<&'static str>) {
    if statuses.is_empty() {
        return (String::new(), Vec::new());
    }
    let placeholders = std::iter::repeat("?")
        .take(statuses.len())
        .collect::<Vec<_>>()
        .join(", ");
    let keyword = if has_where { "AND" } else { "WHERE" };
    let clause = format!(" {keyword} status IN ({placeholders})");
    let binds = statuses.iter().map(|s| s.as_str()).collect();
    (clause, binds)
}

// ── row mapping ───────────────────────────────────────────────────────────────

fn row_to_task(row: &sqlx::sqlite::SqliteRow) -> Result<Task> {
    let id: String = row
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let project_id_s: Option<String> = row
        .try_get("project_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let title: String = row
        .try_get("title")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let description: String = row
        .try_get("description")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let status_s: String = row
        .try_get("status")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let priority_s: String = row
        .try_get("priority")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let due_at_s: Option<String> = row
        .try_get("due_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let created_at_s: String = row
        .try_get("created_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let updated_at_s: String = row
        .try_get("updated_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let started_at_s: Option<String> = row
        .try_get("started_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let completed_at_s: Option<String> = row
        .try_get("completed_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let created_by_json: Option<String> = row
        .try_get("created_by_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let completed_by_json: Option<String> = row
        .try_get("completed_by_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let updated_by_json: Option<String> = row
        .try_get("updated_by_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let updated_event_id_s: Option<String> = row
        .try_get("updated_event_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let updated_event_seq_i: Option<i64> = row
        .try_get("updated_event_seq")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let source_event_id_s: Option<String> = row
        .try_get("source_event_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let triage_state_s: Option<String> = row
        .try_get("triage_state")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    let task_id = id
        .parse::<TaskId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let project_id = project_id_s
        .map(|s| {
            s.parse::<ProjectId>()
                .map_err(|e| CoreError::serde(e.to_string()))
        })
        .transpose()?;

    Ok(Task {
        id: task_id,
        project_id,
        title,
        description,
        status: parse_status(&status_s)?,
        priority: parse_priority(&priority_s)?,
        triage_state: triage_state_s
            .as_deref()
            .map(parse_triage_state)
            .transpose()?,
        due_at: due_at_s.map(|s| parse_ts(&s)).transpose()?,
        created_at: parse_ts(&created_at_s)?,
        updated_at: parse_ts(&updated_at_s)?,
        started_at: started_at_s.map(|s| parse_ts(&s)).transpose()?,
        completed_at: completed_at_s.map(|s| parse_ts(&s)).transpose()?,
        created_by: created_by_json
            .map(|s| serde_json::from_str(&s).map_err(|e| CoreError::serde(e.to_string())))
            .transpose()?,
        completed_by: completed_by_json
            .map(|s| serde_json::from_str(&s).map_err(|e| CoreError::serde(e.to_string())))
            .transpose()?,
        updated_by: updated_by_json
            .map(|s| serde_json::from_str(&s).map_err(|e| CoreError::serde(e.to_string())))
            .transpose()?,
        updated_event_id: updated_event_id_s
            .map(|s| {
                s.parse::<EventId>()
                    .map_err(|e| CoreError::serde(e.to_string()))
            })
            .transpose()?,
        updated_event_seq: updated_event_seq_i.map(|seq| seq as u64),
        source_event_id: source_event_id_s
            .map(|s| {
                s.parse::<EventId>()
                    .map_err(|e| CoreError::serde(e.to_string()))
            })
            .transpose()?,
    })
}

fn stamp_last_change(task: &mut Task, envelope: &EventEnvelope) {
    task.updated_by = Some(envelope.actor.clone());
    task.updated_event_id = Some(envelope.id);
    task.updated_event_seq = Some(envelope.seq);
}

fn parse_status(s: &str) -> Result<Status> {
    match s {
        "inbox" => Ok(Status::Inbox),
        "todo" => Ok(Status::Todo),
        "in_progress" => Ok(Status::InProgress),
        "in_review" => Ok(Status::InReview),
        "done" => Ok(Status::Done),
        "cancelled" => Ok(Status::Cancelled),
        other => Err(CoreError::serde(format!("unknown status: {other}"))),
    }
}

fn parse_triage_state(s: &str) -> Result<TriageState> {
    match s {
        "pending" => Ok(TriageState::Pending),
        "accepted" => Ok(TriageState::Accepted),
        "rejected" => Ok(TriageState::Rejected),
        other => Err(CoreError::serde(format!("unknown triage_state: {other}"))),
    }
}

fn parse_priority(s: &str) -> Result<Priority> {
    match s {
        "p0" => Ok(Priority::P0),
        "p1" => Ok(Priority::P1),
        "p2" => Ok(Priority::P2),
        "p3" => Ok(Priority::P3),
        other => Err(CoreError::serde(format!("unknown priority: {other}"))),
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
    use taskagent_shared::TaskId;

    #[tokio::test]
    async fn task_created_and_retrieved() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = TaskRepo::new(db.pool().clone());

        let id = TaskId::new();
        let new_task = taskagent_domain::NewTask {
            id: Some(id),
            title: "hello".into(),
            ..taskagent_domain::NewTask::new("hello")
        };
        let env = EventEnvelope::new(Actor::user(), Event::TaskCreated { task: new_task });
        repo.apply_event(&env).await.unwrap();

        let task = repo.get(id).await.unwrap().expect("task should exist");
        assert_eq!(task.id, id);
        assert_eq!(task.title, "hello");
        assert!(task.started_at.is_none());
        assert!(task.completed_at.is_none());

        let all = repo.list_all().await.unwrap();
        assert_eq!(all.len(), 1);
    }

    #[tokio::test]
    async fn task_mutation_writes_version_record_once_per_source_event() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = TaskRepo::new(db.pool().clone());

        let id = TaskId::new();
        let new_task = taskagent_domain::NewTask {
            id: Some(id),
            title: "version me".into(),
            ..taskagent_domain::NewTask::new("version me")
        };
        let mut env = EventEnvelope::new(Actor::user(), Event::TaskCreated { task: new_task });
        env.seq = 7;
        let event_id = env.id.to_string();

        repo.apply_event(&env).await.unwrap();
        repo.apply_event(&env).await.unwrap();

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM entity_versions \
             WHERE entity_type = 'task' AND entity_id = ? AND source_event_id = ?",
        )
        .bind(id.to_string())
        .bind(event_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(count, 1, "replaying one event must not duplicate versions");

        let version_number: i64 = sqlx::query_scalar(
            "SELECT version_number FROM entity_versions \
             WHERE entity_type = 'task' AND entity_id = ?",
        )
        .bind(id.to_string())
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(version_number, 1);
    }

    #[tokio::test]
    async fn task_tracks_last_projection_change_event() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = TaskRepo::new(db.pool().clone());

        let id = TaskId::new();
        let new_task = taskagent_domain::NewTask {
            id: Some(id),
            ..taskagent_domain::NewTask::new("audit me")
        };
        let mut create_env =
            EventEnvelope::new(Actor::user(), Event::TaskCreated { task: new_task });
        create_env.seq = 41;
        let create_event_id = create_env.id;
        repo.apply_event(&create_env).await.unwrap();

        let created = repo.get(id).await.unwrap().unwrap();
        assert_eq!(created.updated_by.as_ref(), Some(&Actor::user()));
        assert_eq!(created.updated_event_id, Some(create_event_id));
        assert_eq!(created.updated_event_seq, Some(41));

        let agent = Actor::agent("debugger");
        let mut update_env = EventEnvelope::new(
            agent.clone(),
            Event::TaskPriorityChanged {
                task_id: id,
                from: Priority::P2,
                to: Priority::P1,
            },
        );
        update_env.seq = 42;
        let update_event_id = update_env.id;
        repo.apply_event(&update_env).await.unwrap();

        let updated = repo.get(id).await.unwrap().unwrap();
        assert_eq!(updated.priority, Priority::P1);
        assert_eq!(updated.updated_by.as_ref(), Some(&agent));
        assert_eq!(updated.updated_event_id, Some(update_event_id));
        assert_eq!(updated.updated_event_seq, Some(42));
    }

    #[tokio::test]
    async fn task_closed_sets_completed_at() {
        use taskagent_shared::time;
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = TaskRepo::new(db.pool().clone());

        let id = TaskId::new();
        let new_task = taskagent_domain::NewTask {
            id: Some(id),
            ..taskagent_domain::NewTask::new("close me")
        };
        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated { task: new_task },
        ))
        .await
        .unwrap();

        let closed_at = time::now();
        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::TaskClosed {
                task_id: id,
                by: Actor::user(),
                at: closed_at,
            },
        ))
        .await
        .unwrap();

        let task = repo.get(id).await.unwrap().unwrap();
        assert!(task.completed_at.is_some());
        // started_at is back-filled on close for tasks that never went through InProgress
        assert!(task.started_at.is_some());
    }

    #[tokio::test]
    async fn task_reopened_clears_completed_at() {
        use taskagent_shared::time;
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = TaskRepo::new(db.pool().clone());

        let id = TaskId::new();
        let new_task = taskagent_domain::NewTask {
            id: Some(id),
            ..taskagent_domain::NewTask::new("reopen me")
        };
        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated { task: new_task },
        ))
        .await
        .unwrap();

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::TaskClosed {
                task_id: id,
                by: Actor::user(),
                at: time::now(),
            },
        ))
        .await
        .unwrap();
        assert!(repo.get(id).await.unwrap().unwrap().completed_at.is_some());

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::TaskReopened {
                task_id: id,
                by: Actor::user(),
                at: time::now(),
            },
        ))
        .await
        .unwrap();

        let task = repo.get(id).await.unwrap().unwrap();
        assert!(
            task.completed_at.is_none(),
            "reopen must clear completed_at"
        );
        // started_at must survive the reopen so we keep the original work timestamp
        assert!(
            task.started_at.is_some(),
            "started_at must persist across reopen"
        );
    }

    #[tokio::test]
    async fn list_filtered_by_status_combines_with_project_scope() {
        use taskagent_shared::ProjectId;

        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = TaskRepo::new(db.pool().clone());

        let proj = ProjectId::new();
        let other = ProjectId::new();

        let mk = |title: &str, project: Option<ProjectId>, status: Status| {
            let id = TaskId::new();
            let new_task = taskagent_domain::NewTask {
                id: Some(id),
                project_id: project,
                status: Some(status),
                ..taskagent_domain::NewTask::new(title)
            };
            (id, new_task)
        };

        let cases = [
            mk("todo-A", Some(proj), Status::Todo),
            mk("inprog-A", Some(proj), Status::InProgress),
            mk("done-A", Some(proj), Status::Done),
            mk("todo-B", Some(other), Status::Todo),
            mk("inbox-orphan", None, Status::Inbox),
            mk("done-orphan", None, Status::Done),
        ];
        for (_, t) in &cases {
            repo.apply_event(&EventEnvelope::new(
                Actor::user(),
                Event::TaskCreated { task: t.clone() },
            ))
            .await
            .unwrap();
        }

        // Project + active subset → only non-terminal in `proj`.
        let active = [
            Status::Inbox,
            Status::Todo,
            Status::InProgress,
            Status::InReview,
        ];
        let got = repo
            .list_by_project_filtered(Some(proj), &active)
            .await
            .unwrap();
        let titles: Vec<_> = got.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["todo-A", "inprog-A"]);

        // Empty status slice = no filter (backward-compat).
        let all_in_proj = repo
            .list_by_project_filtered(Some(proj), &[])
            .await
            .unwrap();
        assert_eq!(all_in_proj.len(), 3);

        // Inbox scope (project_id IS NULL) + active.
        let inbox_active = repo.list_by_project_filtered(None, &active).await.unwrap();
        let titles: Vec<_> = inbox_active.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["inbox-orphan"]);

        // Global active across every project (todo-A, inprog-A, todo-B,
        // inbox-orphan — done-A and done-orphan are excluded).
        let global_active = repo.list_all_filtered(&active).await.unwrap();
        assert_eq!(global_active.len(), 4);
    }
}
