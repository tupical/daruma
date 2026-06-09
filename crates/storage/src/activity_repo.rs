//! Activity projection repository — materialises events into the `activity` table.

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use taskagent_domain::{Activity, Actor, Verb};
use taskagent_events::{Event, EventEnvelope, EventStore};
use taskagent_shared::{ActivityId, CoreError, EventId, PlanId, ProjectId, Result, RunId, TaskId};

/// Read/write access to the `activity` projection table.
pub struct ActivityRepo {
    pub(crate) pool: SqlitePool,
}

impl ActivityRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── Queries ────────────────────────────────────────────────────────────

    /// List activity rows for a task with cursor-pagination and optional verb filter.
    ///
    /// Returns `(items, next_cursor, has_more)`.
    /// `cursor` is the last-seen `seq` (exclusive lower bound); `None`/`0` = start.
    /// `limit` is capped to 500.
    pub async fn list_for_task(
        &self,
        task_id: TaskId,
        cursor: Option<u64>,
        limit: u32,
        verbs: Option<&[Verb]>,
    ) -> Result<(Vec<Activity>, Option<u64>, bool)> {
        let since_seq = cursor.unwrap_or(0) as i64;
        let limit = limit.min(500);
        let fetch_limit = limit as i64 + 1;
        let task_id_str = task_id.to_string();

        let rows = if let Some(verb_list) = verbs {
            if verb_list.is_empty() {
                return Ok((vec![], None, false));
            }
            let placeholders = verb_list.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!(
                "SELECT id, task_id, project_id, actor_json, verb, field, old_value, new_value, \
                 occurred_at, event_id, seq \
                 FROM activity WHERE task_id = ? AND seq > ? AND verb IN ({placeholders}) \
                 ORDER BY seq ASC LIMIT ?"
            );
            let mut q = sqlx::query(&sql).bind(task_id_str).bind(since_seq);
            for v in verb_list {
                q = q.bind(v.to_string());
            }
            q = q.bind(fetch_limit);
            q.fetch_all(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?
        } else {
            sqlx::query(
                "SELECT id, task_id, project_id, actor_json, verb, field, old_value, new_value, \
                 occurred_at, event_id, seq \
                 FROM activity WHERE task_id = ? AND seq > ? ORDER BY seq ASC LIMIT ?",
            )
            .bind(task_id_str)
            .bind(since_seq)
            .bind(fetch_limit)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?
        };

        let mut items: Vec<Activity> = rows.iter().map(row_to_activity).collect::<Result<_>>()?;
        let has_more = items.len() > limit as usize;
        if has_more {
            items.pop();
        }
        let next_cursor = if has_more {
            items.last().map(|a| a.seq as u64)
        } else {
            None
        };

        Ok((items, next_cursor, has_more))
    }

    /// Get a single activity row by id.
    pub async fn get(&self, id: ActivityId) -> Result<Option<Activity>> {
        let row = sqlx::query(
            "SELECT id, task_id, project_id, actor_json, verb, field, old_value, new_value, \
             occurred_at, event_id, seq \
             FROM activity WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(row_to_activity).transpose()
    }

    /// Highest `seq` among all activity rows, or `0` if the table is empty.
    /// Used as the cursor start for [`backfill_from_events`].
    pub async fn last_backfilled_seq(&self) -> Result<u64> {
        let row = sqlx::query("SELECT COALESCE(MAX(seq), 0) AS max_seq FROM activity")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let seq: i64 = row
            .try_get("max_seq")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(seq as u64)
    }

    // ── Mutations ──────────────────────────────────────────────────────────

    /// Apply a single event envelope, updating the `activity` projection.
    ///
    /// Idempotent: subsequent calls for the same `event_id` are no-ops via
    /// `INSERT OR IGNORE` on the `event_id UNIQUE` constraint.
    ///
    /// **Pair-merging**: semantic events (`TaskClosed`, `TaskReopened`,
    /// `TaskCommented`) update the preceding mechanical row at `seq − 1` for
    /// the same `task_id` rather than appending a new row.  If no row is found
    /// (legacy / replay), a new row is inserted as fallback.
    pub async fn apply_event(&self, envelope: &EventEnvelope) -> Result<()> {
        let actor = &envelope.actor;
        let occurred_at = envelope.occurred_at;
        let event_id = envelope.id;
        let seq = envelope.seq as i64;

        match &envelope.payload {
            // ── Task lifecycle ────────────────────────────────────────────
            Event::TaskCreated { task } => {
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: task.id,
                    project_id: task.project_id,
                    actor: actor.clone(),
                    verb: Verb::Created,
                    field: None,
                    old_value: None,
                    new_value: Some(task.title.clone()),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::TaskUpdated { task_id, patch } => {
                let project_id = self.inherit_project_id(*task_id).await?;
                let new_value =
                    serde_json::to_string(patch).map_err(|e| CoreError::serde(e.to_string()))?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*task_id),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::Updated,
                    field: None,
                    old_value: None,
                    new_value: Some(new_value),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::TaskStatusChanged { task_id, from, to } => {
                let project_id = self.inherit_project_id(*task_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*task_id),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::StatusChanged,
                    field: Some("status".into()),
                    old_value: Some(from.as_str().to_string()),
                    new_value: Some(to.as_str().to_string()),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::TaskPriorityChanged { task_id, from, to } => {
                let project_id = self.inherit_project_id(*task_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*task_id),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::PriorityChanged,
                    field: Some("priority".into()),
                    old_value: Some(from.as_str().to_string()),
                    new_value: Some(to.as_str().to_string()),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::TaskCompleted { task_id, .. } => {
                let project_id = self.inherit_project_id(*task_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*task_id),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::Completed,
                    field: None,
                    old_value: None,
                    new_value: None,
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::TaskDeleted { task_id } => {
                let project_id = self.inherit_project_id(*task_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*task_id),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::Deleted,
                    field: None,
                    old_value: None,
                    new_value: None,
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::TaskSplitGenerated { parent, .. } => {
                let project_id = self.inherit_project_id(*parent).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*parent),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::SplitGenerated,
                    field: None,
                    old_value: None,
                    new_value: None,
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            // ── Semantic events — pair-merge into the preceding row ────────
            Event::TaskClosed { task_id, .. } => {
                let prev = envelope.seq.saturating_sub(1) as i64;
                if !self
                    .try_update_verb_at_seq(*task_id, prev, Verb::Closed)
                    .await?
                {
                    // Fallback: no mechanical row at seq-1 (legacy events).
                    let project_id = self.inherit_project_id(*task_id).await?;
                    self.insert_row(Activity {
                        id: ActivityId::new(),
                        task_id: Some(*task_id),
                        project_id,
                        actor: actor.clone(),
                        verb: Verb::Closed,
                        field: None,
                        old_value: None,
                        new_value: None,
                        occurred_at,
                        event_id,
                        seq,
                    })
                    .await?;
                }
            }

            Event::TaskReopened { task_id, .. } => {
                let prev = envelope.seq.saturating_sub(1) as i64;
                if !self
                    .try_update_verb_at_seq(*task_id, prev, Verb::Reopened)
                    .await?
                {
                    let project_id = self.inherit_project_id(*task_id).await?;
                    self.insert_row(Activity {
                        id: ActivityId::new(),
                        task_id: Some(*task_id),
                        project_id,
                        actor: actor.clone(),
                        verb: Verb::Reopened,
                        field: None,
                        old_value: None,
                        new_value: None,
                        occurred_at,
                        event_id,
                        seq,
                    })
                    .await?;
                }
            }

            Event::TaskCommented {
                task_id, preview, ..
            } => {
                let prev = envelope.seq.saturating_sub(1) as i64;
                if !self
                    .try_update_commented_at_seq(*task_id, prev, preview)
                    .await?
                {
                    // Fallback: CommentAdded not found (replay-only case).
                    let project_id = self.inherit_project_id(*task_id).await?;
                    self.insert_row(Activity {
                        id: ActivityId::new(),
                        task_id: Some(*task_id),
                        project_id,
                        actor: actor.clone(),
                        verb: Verb::Commented,
                        field: None,
                        old_value: None,
                        new_value: Some(preview.clone()),
                        occurred_at,
                        event_id,
                        seq,
                    })
                    .await?;
                }
            }

            // ── Project events ────────────────────────────────────────────
            Event::ProjectCreated { project } => {
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: None,
                    project_id: Some(project.id),
                    actor: actor.clone(),
                    verb: Verb::ProjectCreated,
                    field: None,
                    old_value: None,
                    new_value: Some(project.title.clone()),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::ProjectUpdated { project_id, .. } => {
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: None,
                    project_id: Some(*project_id),
                    actor: actor.clone(),
                    verb: Verb::ProjectUpdated,
                    field: None,
                    old_value: None,
                    new_value: None,
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::ProjectDeleted { project_id } => {
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: None,
                    project_id: Some(*project_id),
                    actor: actor.clone(),
                    verb: Verb::ProjectDeleted,
                    field: None,
                    old_value: None,
                    new_value: None,
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            // ── Comment events ────────────────────────────────────────────
            Event::CommentAdded { comment } => {
                // Insert initial Commented row; TaskCommented (seq+1) will enrich
                // new_value with its canonical preview via pair-merge.
                let project_id = self.inherit_project_id(comment.task_id).await?;
                let preview: String = comment.body.chars().take(80).collect();
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(comment.task_id),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::Commented,
                    field: None,
                    old_value: None,
                    new_value: Some(preview),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::CommentEdited { task_id, .. } => {
                let project_id = self.inherit_project_id(*task_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*task_id),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::CommentEdited,
                    field: None,
                    old_value: None,
                    new_value: None,
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::CommentDeleted { task_id, .. } => {
                let project_id = self.inherit_project_id(*task_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*task_id),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::CommentDeleted,
                    field: None,
                    old_value: None,
                    new_value: None,
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            // ── Agent events ──────────────────────────────────────────────
            Event::AgentActionRecorded { .. } => {
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: None,
                    project_id: None,
                    actor: actor.clone(),
                    verb: Verb::AgentAction,
                    field: None,
                    old_value: None,
                    new_value: None,
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            // ── Plan events ───────────────────────────────────────────────
            Event::PlanCreated { plan } => {
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: None,
                    project_id: Some(plan.project_id),
                    actor: actor.clone(),
                    verb: Verb::PlanCreated,
                    field: None,
                    old_value: None,
                    new_value: Some(plan.title.clone()),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::PlanUpdated { plan_id, patch } => {
                let project_id = self.inherit_plan_project_id(*plan_id).await?;
                // When parent_plan_id changes, use a dedicated PlanReparented verb so
                // activity consumers can distinguish hierarchy edits from content edits.
                let (verb, field, new_value) = if patch.parent_plan_id.is_some() {
                    let nv = match &patch.parent_plan_id {
                        Some(Some(id)) => Some(id.to_string()),
                        Some(None) => Some("null".to_owned()),
                        None => unreachable!(),
                    };
                    (Verb::PlanReparented, Some("parent_plan_id".to_owned()), nv)
                } else {
                    (Verb::PlanModified, None, None)
                };
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: None,
                    project_id,
                    actor: actor.clone(),
                    verb,
                    field,
                    old_value: None,
                    new_value,
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::PlanReordered { plan_id, .. } => {
                let project_id = self.inherit_plan_project_id(*plan_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: None,
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::PlanModified,
                    field: None,
                    old_value: None,
                    new_value: None,
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::PlanStatusChanged { plan_id, from, to } => {
                let project_id = self.inherit_plan_project_id(*plan_id).await?;
                let old_value = serde_json::to_value(from)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned));
                let new_value = serde_json::to_value(to)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned));
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: None,
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::PlanModified,
                    field: Some("status".into()),
                    old_value,
                    new_value,
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::PlanGoalChanged { plan_id, from, to } => {
                let project_id = self.inherit_plan_project_id(*plan_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: None,
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::PlanModified,
                    field: Some("goal".into()),
                    old_value: Some(from.clone()),
                    new_value: Some(to.clone()),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::PlanTaskAdded {
                plan_id, task_id, ..
            } => {
                let project_id = self.inherit_plan_project_id(*plan_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*task_id),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::TaskAttached,
                    field: None,
                    old_value: None,
                    new_value: Some(plan_id.to_string()),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::PlanTaskRemoved { plan_id, task_id } => {
                let project_id = self.inherit_plan_project_id(*plan_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*task_id),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::TaskDetached,
                    field: None,
                    old_value: None,
                    new_value: Some(plan_id.to_string()),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::PlanArchived { plan_id, .. } => {
                let project_id = self.inherit_plan_project_id(*plan_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: None,
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::PlanArchived,
                    field: None,
                    old_value: None,
                    new_value: None,
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            // ── Run events ────────────────────────────────────────────────
            Event::RunStarted { run } => {
                let project_id = self.inherit_plan_project_id(run.plan_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: None,
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::RunStarted,
                    field: None,
                    old_value: None,
                    new_value: Some(run.id.to_string()),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::RunCompleted { run_id, .. } => {
                let project_id = self.inherit_run_project_id(*run_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: None,
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::RunCompleted,
                    field: None,
                    old_value: None,
                    new_value: None,
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::RunFailed { run_id, reason, .. } => {
                let project_id = self.inherit_run_project_id(*run_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: None,
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::RunFailed,
                    field: None,
                    old_value: None,
                    new_value: Some(reason.clone()),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::RunAborted { run_id, reason, .. } => {
                let project_id = self.inherit_run_project_id(*run_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: None,
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::RunAborted,
                    field: None,
                    old_value: None,
                    new_value: Some(reason.clone()),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            // ── Agent claims ──────────────────────────────────────────────
            Event::AgentClaimed { task_id, .. } => {
                let project_id = self.inherit_project_id(*task_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*task_id),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::TaskClaimed,
                    field: None,
                    old_value: None,
                    new_value: None,
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::AgentReleased { task_id, .. } => {
                let project_id = self.inherit_project_id(*task_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*task_id),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::TaskReleased,
                    field: None,
                    old_value: None,
                    new_value: None,
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            // ── No activity rows for internal / signal events ─────────────
            Event::FilesReserved { .. }
            | Event::FilesReleased { .. }
            | Event::RunStepStarted { .. }
            | Event::RunStepFinished { .. }
            | Event::AgentSessionStarted { .. }
            | Event::AgentSessionEnded { .. }
            | Event::AgentSessionPlanUpdated { .. }
            | Event::SessionArtifactAttached { .. }
            | Event::PlanModifiedByHuman { .. }
            | Event::TaskContested { .. }
            | Event::RunObsolescedByPlanEdit { .. }
            | Event::RunStopRequested { .. }
            | Event::RunElicitationRequested { .. }
            | Event::RunAuthRequired { .. }
            | Event::RunInterventionAccepted { .. }
            | Event::RunUnresponsive { .. }
            | Event::RunStale { .. }
            | Event::RunNoteAppended { .. }
            | Event::ConflictResolved { .. }
            // Documents (PR1): no activity rows — DocumentRepo owns the
            // projection. Activity feed is task-centric.
            | Event::DocumentCreated { .. }
            | Event::DocumentContentReplaced { .. }
            | Event::DocumentContentAppended { .. }
            | Event::DocumentRenamed { .. }
            | Event::DocumentArchived { .. } => {}

            // ── Relation events (§3.2 W2.2) ──────────────────────────────────
            Event::TaskLinked {
                relation_id,
                from,
                to,
                kind,
                ..
            } => {
                let project_id = self.inherit_project_id(*from).await?;
                let payload = serde_json::json!({
                    "relation_id": relation_id.to_string(),
                    "kind": kind,
                    "to": to.to_string(),
                })
                .to_string();
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*from),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::Linked,
                    field: None,
                    old_value: None,
                    new_value: Some(payload),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::TaskUnlinked {
                relation_id,
                from,
                to,
                kind,
                ..
            } => {
                let project_id = self.inherit_project_id(*from).await?;
                let payload = serde_json::json!({
                    "relation_id": relation_id.to_string(),
                    "kind": kind,
                    "to": to.to_string(),
                })
                .to_string();
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*from),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::Unlinked,
                    field: None,
                    old_value: None,
                    new_value: Some(payload),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::TaskUnblocked {
                task_id,
                unblocked_by,
                ..
            } => {
                let project_id = self.inherit_project_id(*task_id).await?;
                let payload = serde_json::json!({
                    "unblocked_by": unblocked_by.to_string(),
                })
                .to_string();
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*task_id),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::Unblocked,
                    field: None,
                    old_value: None,
                    new_value: Some(payload),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            Event::TaskDueElapsed {
                task_id, due_at, ..
            } => {
                let project_id = self.inherit_project_id(*task_id).await?;
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*task_id),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::DueElapsed,
                    field: Some("due_at".into()),
                    old_value: None,
                    new_value: Some(due_at.to_rfc3339()),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }

            // §3.7.2 / LIN A.3 — historical relation transition (Blocks → WasBlocking).
            Event::TaskRelationKindChanged {
                relation_id,
                from,
                to,
                from_kind,
                to_kind,
                ..
            } => {
                let project_id = self.inherit_project_id(*from).await?;
                let payload = serde_json::json!({
                    "relation_id": relation_id.to_string(),
                    "from_kind": from_kind,
                    "to_kind": to_kind,
                    "to": to.to_string(),
                })
                .to_string();
                self.insert_row(Activity {
                    id: ActivityId::new(),
                    task_id: Some(*from),
                    project_id,
                    actor: actor.clone(),
                    verb: Verb::RelationKindChanged,
                    field: Some("kind".into()),
                    old_value: Some(serde_json::to_string(from_kind).unwrap_or_default()),
                    new_value: Some(payload),
                    occurred_at,
                    event_id,
                    seq,
                })
                .await?;
            }
        }

        Ok(())
    }

    /// Look up the `project_id` from the most recent activity row for `task_id`.
    ///
    /// Used to propagate `project_id` to subsequent task events that don't carry
    /// it in their payload (only `TaskCreated` does).  Returns `None` when no
    /// preceding row exists (orphan events during partial-log backfill).
    async fn inherit_project_id(&self, task_id: TaskId) -> Result<Option<ProjectId>> {
        let row = sqlx::query(
            "SELECT project_id FROM activity WHERE task_id = ? \
             ORDER BY seq DESC LIMIT 1",
        )
        .bind(task_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        let Some(row) = row else {
            return Ok(None);
        };
        let pid_s: Option<String> = row
            .try_get("project_id")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        pid_s
            .map(|s| {
                s.parse::<ProjectId>()
                    .map_err(|e| CoreError::serde(e.to_string()))
            })
            .transpose()
    }

    /// Look up the `project_id` for a plan by `plan_id`.
    ///
    /// Returns `None` when the plan is not yet in the DB (partial-log replay).
    async fn inherit_plan_project_id(&self, plan_id: PlanId) -> Result<Option<ProjectId>> {
        let row = sqlx::query("SELECT project_id FROM plans WHERE id = ?")
            .bind(plan_id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let pid_s: String = row
            .try_get("project_id")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        pid_s
            .parse::<ProjectId>()
            .map(Some)
            .map_err(|e| CoreError::serde(e.to_string()))
    }

    /// Look up the `project_id` for a run by joining `runs` → `plans`.
    ///
    /// Returns `None` when the run or its plan is not yet in the DB.
    async fn inherit_run_project_id(&self, run_id: RunId) -> Result<Option<ProjectId>> {
        let row = sqlx::query(
            "SELECT p.project_id FROM plans p \
             JOIN runs r ON r.plan_id = p.id \
             WHERE r.id = ?",
        )
        .bind(run_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let pid_s: String = row
            .try_get("project_id")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        pid_s
            .parse::<ProjectId>()
            .map(Some)
            .map_err(|e| CoreError::serde(e.to_string()))
    }

    /// Replay all events since the last backfilled `seq`, inserting activity rows.
    ///
    /// Idempotent: duplicate `event_id`s are silently ignored by `INSERT OR IGNORE`.
    /// Call once after `Db::migrate()`, before starting the dispatcher.
    pub async fn backfill_from_events(&self, store: &dyn EventStore) -> Result<u64> {
        let mut last = self.last_backfilled_seq().await?;
        let mut count = 0u64;
        loop {
            let batch = store.load_since(last, 1000).await?;
            if batch.is_empty() {
                break;
            }
            for env in &batch {
                self.apply_event(env).await?;
                count += 1;
            }
            // SAFETY: batch is non-empty; unwrap cannot panic.
            last = batch.last().unwrap().seq;
        }
        Ok(count)
    }

    // ── Private helpers ────────────────────────────────────────────────────

    /// Insert an activity row; silently ignores duplicate `event_id`.
    async fn insert_row(&self, a: Activity) -> Result<()> {
        let actor_json =
            serde_json::to_string(&a.actor).map_err(|e| CoreError::serde(e.to_string()))?;
        sqlx::query(
            "INSERT OR IGNORE INTO activity \
             (id, task_id, project_id, actor_json, verb, field, old_value, new_value, \
              occurred_at, event_id, seq) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(a.id.to_string())
        .bind(a.task_id.map(|t| t.to_string()))
        .bind(a.project_id.map(|p| p.to_string()))
        .bind(actor_json)
        .bind(a.verb.to_string())
        .bind(a.field)
        .bind(a.old_value)
        .bind(a.new_value)
        .bind(a.occurred_at.to_rfc3339())
        .bind(a.event_id.to_string())
        .bind(a.seq)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Pair-merge: update the verb of the activity row at `(task_id, seq)`.
    /// Returns `true` if a row was found and updated.
    async fn try_update_verb_at_seq(&self, task_id: TaskId, seq: i64, verb: Verb) -> Result<bool> {
        let n = sqlx::query("UPDATE activity SET verb = ? WHERE task_id = ? AND seq = ?")
            .bind(verb.to_string())
            .bind(task_id.to_string())
            .bind(seq)
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?
            .rows_affected();
        Ok(n > 0)
    }

    /// Pair-merge for `TaskCommented`: update `new_value` (preview) of the
    /// `CommentAdded` row at `(task_id, seq)`.  Returns `true` if updated.
    async fn try_update_commented_at_seq(
        &self,
        task_id: TaskId,
        seq: i64,
        preview: &str,
    ) -> Result<bool> {
        let n = sqlx::query(
            "UPDATE activity SET verb = 'commented', new_value = ? \
             WHERE task_id = ? AND seq = ?",
        )
        .bind(preview)
        .bind(task_id.to_string())
        .bind(seq)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?
        .rows_affected();
        Ok(n > 0)
    }
}

// ── Row mapping ───────────────────────────────────────────────────────────────

fn row_to_activity(row: &sqlx::sqlite::SqliteRow) -> Result<Activity> {
    let id_s: String = row
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let task_id_s: Option<String> = row
        .try_get("task_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let project_id_s: Option<String> = row
        .try_get("project_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let actor_json: String = row
        .try_get("actor_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let verb_s: String = row
        .try_get("verb")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let field: Option<String> = row
        .try_get("field")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let old_value: Option<String> = row
        .try_get("old_value")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let new_value: Option<String> = row
        .try_get("new_value")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let occurred_at_s: String = row
        .try_get("occurred_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let event_id_s: String = row
        .try_get("event_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let seq: i64 = row
        .try_get("seq")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    let id = id_s
        .parse::<ActivityId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let task_id = task_id_s
        .map(|s| {
            s.parse::<TaskId>()
                .map_err(|e| CoreError::serde(e.to_string()))
        })
        .transpose()?;
    let project_id = project_id_s
        .map(|s| {
            s.parse::<ProjectId>()
                .map_err(|e| CoreError::serde(e.to_string()))
        })
        .transpose()?;
    let actor: Actor =
        serde_json::from_str(&actor_json).map_err(|e| CoreError::serde(e.to_string()))?;
    let verb: Verb = verb_s.parse().map_err(|e: String| CoreError::serde(e))?;
    let occurred_at: DateTime<Utc> = DateTime::parse_from_rfc3339(&occurred_at_s)
        .map_err(|e| CoreError::serde(e.to_string()))?
        .with_timezone(&Utc);
    let event_id = event_id_s
        .parse::<EventId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;

    Ok(Activity {
        id,
        task_id,
        project_id,
        actor,
        verb,
        field,
        old_value,
        new_value,
        occurred_at,
        event_id,
        seq,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, SqliteEventStore};
    use taskagent_domain::{Actor, Comment, NewComment, NewTask, Status, Verb};
    use taskagent_events::{Event, EventEnvelope, EventStore};
    use taskagent_shared::{time, CommentId, TaskId};

    async fn make_repo() -> ActivityRepo {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        ActivityRepo::new(db.pool().clone())
    }

    async fn make_repo_with_store() -> (ActivityRepo, SqliteEventStore) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let pool = db.pool().clone();
        let repo = ActivityRepo::new(pool.clone());
        let store = SqliteEventStore::new(pool);
        (repo, store)
    }

    fn env_with_seq(actor: Actor, payload: Event, seq: u64) -> EventEnvelope {
        EventEnvelope {
            seq,
            ..EventEnvelope::new(actor, payload)
        }
    }

    // ── 1. TaskCreated ────────────────────────────────────────────────────

    #[tokio::test]
    async fn apply_task_created_writes_row() {
        let repo = make_repo().await;
        let task_id = TaskId::new();
        let task = NewTask {
            id: Some(task_id),
            ..NewTask::new("smoke test")
        };
        let env = env_with_seq(Actor::user(), Event::TaskCreated { task }, 1);
        repo.apply_event(&env).await.unwrap();

        let (rows, next, has_more) = repo.list_for_task(task_id, None, 10, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verb, Verb::Created);
        assert_eq!(rows[0].new_value.as_deref(), Some("smoke test"));
        assert!(!has_more);
        assert!(next.is_none());
    }

    // ── 2. TaskStatusChanged ──────────────────────────────────────────────

    #[tokio::test]
    async fn apply_status_changed_writes_status_changed_verb() {
        let repo = make_repo().await;
        let task_id = TaskId::new();
        let env = env_with_seq(
            Actor::user(),
            Event::TaskStatusChanged {
                task_id,
                from: Status::Todo,
                to: Status::InProgress,
            },
            1,
        );
        repo.apply_event(&env).await.unwrap();

        let (rows, _, _) = repo.list_for_task(task_id, None, 10, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verb, Verb::StatusChanged);
        assert_eq!(rows[0].field.as_deref(), Some("status"));
        assert_eq!(rows[0].old_value.as_deref(), Some("todo"));
        assert_eq!(rows[0].new_value.as_deref(), Some("in_progress"));
    }

    // ── 3. Pair-merge: StatusChanged + TaskClosed → Closed ────────────────

    #[tokio::test]
    async fn apply_status_change_then_closed_merges_into_closed_verb() {
        let repo = make_repo().await;
        let task_id = TaskId::new();

        // Mechanical event at seq=1
        repo.apply_event(&env_with_seq(
            Actor::user(),
            Event::TaskStatusChanged {
                task_id,
                from: Status::Todo,
                to: Status::Done,
            },
            1,
        ))
        .await
        .unwrap();

        // Semantic event at seq=2 → should UPDATE the seq=1 row
        repo.apply_event(&env_with_seq(
            Actor::user(),
            Event::TaskClosed {
                task_id,
                by: Actor::user(),
                at: time::now(),
            },
            2,
        ))
        .await
        .unwrap();

        let (rows, _, _) = repo.list_for_task(task_id, None, 10, None).await.unwrap();
        assert_eq!(rows.len(), 1, "pair-merge must produce exactly one row");
        assert_eq!(rows[0].verb, Verb::Closed);
    }

    // ── 4. Pair-merge: CommentAdded + TaskCommented → Commented ──────────

    #[tokio::test]
    async fn apply_comment_added_then_task_commented_merges_into_commented_verb() {
        let repo = make_repo().await;
        let task_id = TaskId::new();
        let comment_id = CommentId::new();

        let comment = Comment::from_new(
            NewComment {
                id: Some(comment_id),
                task_id,
                body: "looks good to me".to_string(),
                parent_id: None,
                kind: None,
            },
            Actor::user(),
            time::now(),
        );

        // CommentAdded at seq=5
        repo.apply_event(&env_with_seq(
            Actor::user(),
            Event::CommentAdded { comment },
            5,
        ))
        .await
        .unwrap();

        // TaskCommented at seq=6 → pair-merge into seq=5 row
        let preview = "looks good to me".to_string();
        repo.apply_event(&env_with_seq(
            Actor::user(),
            Event::TaskCommented {
                task_id,
                comment_id,
                author: Actor::user(),
                preview: preview.clone(),
            },
            6,
        ))
        .await
        .unwrap();

        let (rows, _, _) = repo.list_for_task(task_id, None, 10, None).await.unwrap();
        assert_eq!(rows.len(), 1, "pair-merge must produce exactly one row");
        assert_eq!(rows[0].verb, Verb::Commented);
        assert_eq!(rows[0].new_value.as_deref(), Some("looks good to me"));
    }

    // ── 5. Pair-merge fallback: only semantic event (legacy replay) ────────

    #[tokio::test]
    async fn apply_semantic_event_without_mechanical_inserts_fallback_row() {
        let repo = make_repo().await;
        let task_id = TaskId::new();

        // TaskClosed at seq=1 with no preceding row → fallback INSERT
        repo.apply_event(&env_with_seq(
            Actor::user(),
            Event::TaskClosed {
                task_id,
                by: Actor::user(),
                at: time::now(),
            },
            1,
        ))
        .await
        .unwrap();

        let (rows, _, _) = repo.list_for_task(task_id, None, 10, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verb, Verb::Closed);
    }

    // ── 6. Backfill: fresh DB ─────────────────────────────────────────────

    #[tokio::test]
    async fn backfill_from_empty() {
        let (repo, store) = make_repo_with_store().await;
        let count = repo.backfill_from_events(&store).await.unwrap();
        assert_eq!(count, 0);
    }

    // ── 7. Backfill: idempotent ───────────────────────────────────────────

    #[tokio::test]
    async fn backfill_idempotent() {
        let (repo, store) = make_repo_with_store().await;
        let task_id = TaskId::new();
        let task = NewTask {
            id: Some(task_id),
            ..NewTask::new("backfill me")
        };

        store
            .append(EventEnvelope::new(
                Actor::user(),
                Event::TaskCreated { task },
            ))
            .await
            .unwrap();

        // First pass
        let count1 = repo.backfill_from_events(&store).await.unwrap();
        assert_eq!(count1, 1);

        // Second pass: last_backfilled_seq == event seq → load_since returns empty
        let count2 = repo.backfill_from_events(&store).await.unwrap();
        assert_eq!(count2, 0);

        // No duplicates
        let (rows, _, _) = repo.list_for_task(task_id, None, 10, None).await.unwrap();
        assert_eq!(rows.len(), 1);
    }

    // ── 8. Cursor pagination ──────────────────────────────────────────────

    #[tokio::test]
    async fn cursor_pagination_returns_next_cursor_and_has_more() {
        let repo = make_repo().await;
        let task_id = TaskId::new();

        // Three distinct events (each has its own EventId → no IGNORE collision)
        for i in 1_u64..=3 {
            repo.apply_event(&env_with_seq(
                Actor::user(),
                Event::TaskStatusChanged {
                    task_id,
                    from: Status::Inbox,
                    to: Status::Todo,
                },
                i,
            ))
            .await
            .unwrap();
        }

        // Page 1: limit=2
        let (page1, next, has_more) = repo.list_for_task(task_id, None, 2, None).await.unwrap();
        assert_eq!(page1.len(), 2);
        assert!(has_more);
        assert_eq!(next, Some(2));

        // Page 2: cursor=2 → should return only seq=3
        let (page2, next2, has_more2) =
            repo.list_for_task(task_id, Some(2), 2, None).await.unwrap();
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].seq, 3);
        assert!(!has_more2);
        assert_eq!(next2, None);
    }

    // ── 9. Verb filter ────────────────────────────────────────────────────

    #[tokio::test]
    async fn verb_filter_returns_only_matching() {
        let repo = make_repo().await;
        let task_id = TaskId::new();

        repo.apply_event(&env_with_seq(
            Actor::user(),
            Event::TaskStatusChanged {
                task_id,
                from: Status::Inbox,
                to: Status::Todo,
            },
            1,
        ))
        .await
        .unwrap();

        repo.apply_event(&env_with_seq(
            Actor::user(),
            Event::TaskDeleted { task_id },
            2,
        ))
        .await
        .unwrap();

        // Filter: only Deleted
        let (rows, _, _) = repo
            .list_for_task(task_id, None, 10, Some(&[Verb::Deleted]))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verb, Verb::Deleted);

        // Filter: only StatusChanged
        let (rows, _, _) = repo
            .list_for_task(task_id, None, 10, Some(&[Verb::StatusChanged]))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verb, Verb::StatusChanged);

        // No filter → both
        let (rows, _, _) = repo.list_for_task(task_id, None, 10, None).await.unwrap();
        assert_eq!(rows.len(), 2);
    }

    // ── W2.3: Plan / Run / Agent events ──────────────────────────────────

    /// Insert a minimal row into `plans` so `inherit_plan_project_id` can resolve it.
    async fn seed_plan(
        pool: &sqlx::SqlitePool,
        plan_id: taskagent_shared::PlanId,
        project_id: ProjectId,
    ) {
        let now = time::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO plans \
             (id, project_id, title, description, goal, success_criteria_json, \
              status, owner_json, created_at, updated_at) \
             VALUES (?, ?, 'test', '', '', '[]', 'draft', '{\"type\":\"user\"}', ?, ?)",
        )
        .bind(plan_id.to_string())
        .bind(project_id.to_string())
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Insert a minimal row into `runs` so `inherit_run_project_id` can resolve it.
    #[allow(dead_code)]
    async fn seed_run(
        pool: &sqlx::SqlitePool,
        run_id: taskagent_shared::RunId,
        plan_id: taskagent_shared::PlanId,
    ) {
        let now = time::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO runs (id, plan_id, agent_id, started_at, status) \
             VALUES (?, ?, 'agent-test', ?, 'active')",
        )
        .bind(run_id.to_string())
        .bind(plan_id.to_string())
        .bind(&now)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn plan_created_writes_plan_row() {
        use taskagent_domain::{Plan, PlanStatus};
        use taskagent_shared::{PlanId, ProjectId};

        let repo = make_repo().await;
        let plan_id = PlanId::new();
        let project_id = ProjectId::new();
        let now = time::now();
        let plan = Plan {
            id: plan_id,
            project_id,
            parent_plan_id: None,
            title: "first plan".to_string(),
            description: String::new(),
            goal: String::new(),
            success_criteria: vec![],
            status: PlanStatus::Draft,
            owner: Actor::user(),
            created_at: now,
            updated_at: now,
            archived_at: None,
            source_brief: None,
        };
        let env = env_with_seq(Actor::user(), Event::PlanCreated { plan }, 1);
        repo.apply_event(&env).await.unwrap();

        let row = sqlx::query("SELECT verb, project_id, task_id FROM activity WHERE seq = 1")
            .fetch_one(&repo.pool)
            .await
            .unwrap();
        let verb: String = row.try_get("verb").unwrap();
        let pid: Option<String> = row.try_get("project_id").unwrap();
        let tid: Option<String> = row.try_get("task_id").unwrap();
        assert_eq!(verb, "plan_created");
        assert_eq!(pid.as_deref(), Some(project_id.to_string().as_str()));
        assert!(tid.is_none(), "plan_created row must not have task_id");
    }

    #[tokio::test]
    async fn plan_task_added_writes_task_attached_row() {
        use taskagent_shared::{PlanId, ProjectId};

        let repo = make_repo().await;
        let plan_id = PlanId::new();
        let project_id = ProjectId::new();
        let task_id = TaskId::new();
        seed_plan(&repo.pool, plan_id, project_id).await;

        let env = env_with_seq(
            Actor::user(),
            Event::PlanTaskAdded {
                plan_id,
                task_id,
                position: 0,
                depends_on: vec![],
            },
            1,
        );
        repo.apply_event(&env).await.unwrap();

        let (rows, _, _) = repo.list_for_task(task_id, None, 10, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verb, Verb::TaskAttached);
        assert_eq!(rows[0].project_id, Some(project_id));
        assert_eq!(
            rows[0].new_value.as_deref(),
            Some(plan_id.to_string().as_str())
        );
    }

    #[tokio::test]
    async fn agent_claimed_writes_task_claimed_row() {
        use taskagent_shared::AgentId;

        let repo = make_repo().await;
        let task_id = TaskId::new();
        let agent_id = AgentId::new();

        let env = env_with_seq(
            Actor::user(),
            Event::AgentClaimed {
                agent_id,
                task_id,
                expires_at: time::now(),
            },
            1,
        );
        repo.apply_event(&env).await.unwrap();

        let (rows, _, _) = repo.list_for_task(task_id, None, 10, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verb, Verb::TaskClaimed);
    }

    #[tokio::test]
    async fn run_started_writes_run_started_row() {
        use taskagent_domain::Run;
        use taskagent_shared::{AgentId, PlanId, ProjectId, RunId};

        let repo = make_repo().await;
        let plan_id = PlanId::new();
        let project_id = ProjectId::new();
        seed_plan(&repo.pool, plan_id, project_id).await;

        let run_id = RunId::new();
        let run = Run {
            id: run_id,
            plan_id,
            agent_id: AgentId::new(),
            parent_run_id: None,
            started_at: time::now(),
            ended_at: None,
            status: taskagent_domain::RunStatus::Active,
            outcome: None,
            last_activity_at: None,
            unresponsive_at: None,
            stale_at: None,
        };
        let env = env_with_seq(Actor::user(), Event::RunStarted { run }, 1);
        repo.apply_event(&env).await.unwrap();

        let row =
            sqlx::query("SELECT verb, project_id, task_id, new_value FROM activity WHERE seq = 1")
                .fetch_one(&repo.pool)
                .await
                .unwrap();
        let verb: String = row.try_get("verb").unwrap();
        let pid: Option<String> = row.try_get("project_id").unwrap();
        let tid: Option<String> = row.try_get("task_id").unwrap();
        let nv: Option<String> = row.try_get("new_value").unwrap();
        assert_eq!(verb, "run_started");
        assert_eq!(pid.as_deref(), Some(project_id.to_string().as_str()));
        assert!(tid.is_none());
        assert_eq!(nv.as_deref(), Some(run_id.to_string().as_str()));
    }

    // ── W2.2: Relation verb mapping (AC-7) ───────────────────────────────────

    #[tokio::test]
    async fn verb_mapping_relations() {
        use taskagent_domain::{Actor, RelationKind};
        use taskagent_shared::{time, RelationId};

        let repo = make_repo().await;
        let from_id = TaskId::new();
        let to_id = TaskId::new();
        let relation_id = RelationId::new();
        let now = time::now();

        // TaskLinked → verb = 'linked', subject = from, object payload contains relation_id.
        let env_linked = env_with_seq(
            Actor::user(),
            Event::TaskLinked {
                relation_id,
                from: from_id,
                to: to_id,
                kind: RelationKind::Blocks,
                actor: Actor::user(),
                occurred_at: now,
            },
            1,
        );
        repo.apply_event(&env_linked).await.unwrap();

        let (rows, _, _) = repo.list_for_task(from_id, None, 10, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verb, Verb::Linked);
        assert!(
            rows[0]
                .new_value
                .as_deref()
                .unwrap_or("")
                .contains(&relation_id.to_string()),
            "linked new_value must contain relation_id"
        );

        // TaskUnlinked → verb = 'unlinked', subject = from.
        let env_unlinked = env_with_seq(
            Actor::user(),
            Event::TaskUnlinked {
                relation_id,
                from: from_id,
                to: to_id,
                kind: RelationKind::Blocks,
                occurred_at: now,
            },
            2,
        );
        repo.apply_event(&env_unlinked).await.unwrap();

        let (rows2, _, _) = repo.list_for_task(from_id, None, 10, None).await.unwrap();
        assert_eq!(rows2.len(), 2);
        assert_eq!(rows2[1].verb, Verb::Unlinked);

        // TaskUnblocked → verb = 'unblocked', subject = task_id.
        let blocker_id = TaskId::new();
        let env_unblocked = env_with_seq(
            Actor::user(),
            Event::TaskUnblocked {
                task_id: from_id,
                unblocked_by: blocker_id,
                occurred_at: now,
            },
            3,
        );
        repo.apply_event(&env_unblocked).await.unwrap();

        let (rows3, _, _) = repo.list_for_task(from_id, None, 10, None).await.unwrap();
        assert_eq!(rows3.len(), 3);
        assert_eq!(rows3[2].verb, Verb::Unblocked);
        assert!(
            rows3[2]
                .new_value
                .as_deref()
                .unwrap_or("")
                .contains(&blocker_id.to_string()),
            "unblocked new_value must contain unblocked_by task id"
        );
    }

    // ── W2: PlanReparented verb ───────────────────────────────────────────────

    #[tokio::test]
    async fn plan_updated_with_parent_change_writes_plan_reparented_row() {
        use taskagent_domain::PlanPatch;
        use taskagent_shared::{PlanId, ProjectId};

        let repo = make_repo().await;
        let plan_id = PlanId::new();
        let project_id = ProjectId::new();
        let parent_id = PlanId::new();
        seed_plan(&repo.pool, plan_id, project_id).await;

        let env = env_with_seq(
            Actor::user(),
            Event::PlanUpdated {
                plan_id,
                patch: PlanPatch {
                    title: None,
                    description: None,
                    goal: None,
                    success_criteria: None,
                    parent_plan_id: Some(Some(parent_id)),
                },
            },
            1,
        );
        repo.apply_event(&env).await.unwrap();

        let row = sqlx::query("SELECT verb, field, new_value FROM activity WHERE seq = 1")
            .fetch_one(&repo.pool)
            .await
            .unwrap();
        let verb: String = row.try_get("verb").unwrap();
        let field: Option<String> = row.try_get("field").unwrap();
        let new_value: Option<String> = row.try_get("new_value").unwrap();
        assert_eq!(
            verb, "plan_reparented",
            "re-parent must use PlanReparented verb"
        );
        assert_eq!(field.as_deref(), Some("parent_plan_id"));
        assert_eq!(
            new_value.as_deref(),
            Some(parent_id.to_string().as_str()),
            "new_value must contain the new parent id"
        );
    }

    #[tokio::test]
    async fn plan_updated_without_parent_change_writes_plan_modified_row() {
        use taskagent_domain::PlanPatch;
        use taskagent_shared::{PlanId, ProjectId};

        let repo = make_repo().await;
        let plan_id = PlanId::new();
        let project_id = ProjectId::new();
        seed_plan(&repo.pool, plan_id, project_id).await;

        let env = env_with_seq(
            Actor::user(),
            Event::PlanUpdated {
                plan_id,
                patch: PlanPatch {
                    title: Some("New title".into()),
                    description: None,
                    goal: None,
                    success_criteria: None,
                    parent_plan_id: None,
                },
            },
            1,
        );
        repo.apply_event(&env).await.unwrap();

        let row = sqlx::query("SELECT verb FROM activity WHERE seq = 1")
            .fetch_one(&repo.pool)
            .await
            .unwrap();
        let verb: String = row.try_get("verb").unwrap();
        assert_eq!(
            verb, "plan_modified",
            "title-only update must use PlanModified verb"
        );
    }

    #[tokio::test]
    async fn plan_updated_unparent_writes_plan_reparented_row_with_null() {
        use taskagent_domain::PlanPatch;
        use taskagent_shared::{PlanId, ProjectId};

        let repo = make_repo().await;
        let plan_id = PlanId::new();
        let project_id = ProjectId::new();
        seed_plan(&repo.pool, plan_id, project_id).await;

        let env = env_with_seq(
            Actor::user(),
            Event::PlanUpdated {
                plan_id,
                patch: PlanPatch {
                    title: None,
                    description: None,
                    goal: None,
                    success_criteria: None,
                    parent_plan_id: Some(None), // explicit unparent
                },
            },
            1,
        );
        repo.apply_event(&env).await.unwrap();

        let row = sqlx::query("SELECT verb, new_value FROM activity WHERE seq = 1")
            .fetch_one(&repo.pool)
            .await
            .unwrap();
        let verb: String = row.try_get("verb").unwrap();
        let new_value: Option<String> = row.try_get("new_value").unwrap();
        assert_eq!(verb, "plan_reparented");
        assert_eq!(
            new_value.as_deref(),
            Some("null"),
            "unparent must record new_value = 'null'"
        );
    }

    #[tokio::test]
    async fn plan_modified_by_human_writes_no_row() {
        use taskagent_shared::PlanId;

        let repo = make_repo().await;
        let plan_id = PlanId::new();

        let env = env_with_seq(
            Actor::user(),
            Event::PlanModifiedByHuman {
                plan_id,
                during_run_id: None,
            },
            1,
        );
        repo.apply_event(&env).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM activity")
            .fetch_one(&repo.pool)
            .await
            .unwrap();
        assert_eq!(
            count, 0,
            "PlanModifiedByHuman must not produce an activity row"
        );
    }

    // ── 10. project_id inheritance ────────────────────────────────────────

    #[tokio::test]
    async fn subsequent_task_events_inherit_project_id_from_created_row() {
        use taskagent_shared::ProjectId;

        let repo = make_repo().await;
        let task_id = TaskId::new();
        let project_id = ProjectId::new();

        // TaskCreated carries the project_id
        let task = NewTask {
            id: Some(task_id),
            project_id: Some(project_id),
            ..NewTask::new("inherit test")
        };
        repo.apply_event(&env_with_seq(Actor::user(), Event::TaskCreated { task }, 1))
            .await
            .unwrap();

        // Subsequent event without project_id in payload
        repo.apply_event(&env_with_seq(
            Actor::user(),
            Event::TaskStatusChanged {
                task_id,
                from: Status::Todo,
                to: Status::Done,
            },
            2,
        ))
        .await
        .unwrap();

        let (rows, _, _) = repo.list_for_task(task_id, None, 10, None).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].project_id,
            Some(project_id),
            "created row must have project_id"
        );
        assert_eq!(
            rows[1].project_id,
            Some(project_id),
            "status_changed row must inherit project_id from created row"
        );
    }
}
