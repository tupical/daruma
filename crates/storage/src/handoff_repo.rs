//! Handoff contract repository — projection over `handoff_contracts`
//! (migration 0043), fed by `HandoffRequested` / `HandoffAccepted` /
//! `HandoffRejected` events. The event log stays the source of truth.
//!
//! One live row per `(from_work_unit, to_work_unit)` pair: `HandoffRequested`
//! upserts by id (a re-request after rejection carries the same id and
//! resets the row to `open`). The `work_unit_drain_next` gate consults
//! [`HandoffRepo`]-owned rows via the SQL predicate in `work_unit_repo` —
//! both projections live in the same SQLite file.

use daruma_domain::{HandoffContract, HandoffStatus};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{CoreError, HandoffId, Result, WorkUnitId};
use sqlx::{Row, SqlitePool};

pub struct HandoffRepo {
    pool: SqlitePool,
}

impl HandoffRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── queries ────────────────────────────────────────────────────────────────

    pub async fn get(&self, id: HandoffId) -> Result<Option<HandoffContract>> {
        let row = sqlx::query(&select_sql("WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        row.as_ref().map(row_to_contract).transpose()
    }

    /// The live contract for a `(from, to)` pair, if any.
    pub async fn get_by_pair(
        &self,
        from: WorkUnitId,
        to: WorkUnitId,
    ) -> Result<Option<HandoffContract>> {
        let row = sqlx::query(&select_sql(
            "WHERE from_work_unit_id = ? AND to_work_unit_id = ?",
        ))
        .bind(from.to_string())
        .bind(to.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        row.as_ref().map(row_to_contract).transpose()
    }

    /// All contracts touching a work unit (either side), newest first —
    /// the "handoff state visible" surface.
    pub async fn list_for_work_unit(&self, id: WorkUnitId) -> Result<Vec<HandoffContract>> {
        let rows = sqlx::query(&select_sql(
            "WHERE from_work_unit_id = ? OR to_work_unit_id = ? \
             ORDER BY updated_at DESC, id",
        ))
        .bind(id.to_string())
        .bind(id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.iter().map(row_to_contract).collect()
    }

    // ── mutations ────────────────────────────────────────────────────────────

    /// Apply a persisted handoff event to the projection. Unknown ids on
    /// accept/reject are no-ops (out-of-order replay tolerance, same policy
    /// as the document projector).
    pub async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        match &env.payload {
            Event::HandoffRequested { handoff } => self.upsert(handoff).await,
            Event::HandoffAccepted {
                handoff_id,
                by,
                notes,
                at,
            } => {
                sqlx::query(
                    "UPDATE handoff_contracts SET \
                        status = 'accepted', accepted_by_agent_id = ?, notes = ?, \
                        required_changes = '[]', updated_at = ? \
                     WHERE id = ?",
                )
                .bind(by.as_ref().map(|a| a.to_string()))
                .bind(notes.as_deref())
                .bind(at.to_rfc3339())
                .bind(handoff_id.to_string())
                .execute(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(())
            }
            Event::HandoffRejected {
                handoff_id,
                reason,
                required_changes,
                at,
            } => {
                let changes = serde_json::to_string(required_changes)
                    .map_err(|e| CoreError::serde(e.to_string()))?;
                sqlx::query(
                    "UPDATE handoff_contracts SET \
                        status = 'rejected', notes = ?, required_changes = ?, updated_at = ? \
                     WHERE id = ?",
                )
                .bind(reason)
                .bind(changes)
                .bind(at.to_rfc3339())
                .bind(handoff_id.to_string())
                .execute(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn upsert(&self, h: &HandoffContract) -> Result<()> {
        let artifacts = serde_json::to_string(&h.required_artifact_ids)
            .map_err(|e| CoreError::serde(e.to_string()))?;
        let checklist =
            serde_json::to_string(&h.checklist).map_err(|e| CoreError::serde(e.to_string()))?;
        let changes = serde_json::to_string(&h.required_changes)
            .map_err(|e| CoreError::serde(e.to_string()))?;
        sqlx::query(
            "INSERT INTO handoff_contracts \
             (id, from_work_unit_id, to_work_unit_id, required_artifact_ids, required_state, \
              checklist, owner_agent_id, accepted_by_agent_id, status, notes, required_changes, \
              created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                required_artifact_ids = excluded.required_artifact_ids, \
                required_state = excluded.required_state, \
                checklist = excluded.checklist, \
                owner_agent_id = excluded.owner_agent_id, \
                accepted_by_agent_id = excluded.accepted_by_agent_id, \
                status = excluded.status, \
                notes = excluded.notes, \
                required_changes = excluded.required_changes, \
                updated_at = excluded.updated_at",
        )
        .bind(h.id.to_string())
        .bind(h.from_work_unit_id.to_string())
        .bind(h.to_work_unit_id.to_string())
        .bind(artifacts)
        .bind(h.required_state.as_deref())
        .bind(checklist)
        .bind(h.owner_agent_id.as_ref().map(|a| a.to_string()))
        .bind(h.accepted_by_agent_id.as_ref().map(|a| a.to_string()))
        .bind(h.status.as_str())
        .bind(h.notes.as_deref())
        .bind(changes)
        .bind(h.created_at.to_rfc3339())
        .bind(h.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }
}

fn select_sql(filter: &str) -> String {
    format!(
        "SELECT id, from_work_unit_id, to_work_unit_id, required_artifact_ids, required_state, \
         checklist, owner_agent_id, accepted_by_agent_id, status, notes, required_changes, \
         created_at, updated_at \
         FROM handoff_contracts {filter}"
    )
}

fn row_to_contract(row: &sqlx::sqlite::SqliteRow) -> Result<HandoffContract> {
    fn col<T>(v: std::result::Result<T, sqlx::Error>) -> Result<T> {
        v.map_err(|e| CoreError::storage(e.to_string()))
    }
    fn parse_vec(s: &str) -> Result<Vec<String>> {
        serde_json::from_str(s).map_err(|e| CoreError::serde(e.to_string()))
    }

    let id: String = col(row.try_get("id"))?;
    let from_s: String = col(row.try_get("from_work_unit_id"))?;
    let to_s: String = col(row.try_get("to_work_unit_id"))?;
    let artifacts_s: String = col(row.try_get("required_artifact_ids"))?;
    let required_state: Option<String> = col(row.try_get("required_state"))?;
    let checklist_s: String = col(row.try_get("checklist"))?;
    let owner_s: Option<String> = col(row.try_get("owner_agent_id"))?;
    let accepted_s: Option<String> = col(row.try_get("accepted_by_agent_id"))?;
    let status_s: String = col(row.try_get("status"))?;
    let notes: Option<String> = col(row.try_get("notes"))?;
    let changes_s: String = col(row.try_get("required_changes"))?;
    let created_at_s: String = col(row.try_get("created_at"))?;
    let updated_at_s: String = col(row.try_get("updated_at"))?;

    let status = HandoffStatus::parse(&status_s)
        .ok_or_else(|| CoreError::serde(format!("unknown handoff status: {status_s:?}")))?;

    Ok(HandoffContract {
        id: id
            .parse::<HandoffId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        from_work_unit_id: from_s
            .parse::<WorkUnitId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        to_work_unit_id: to_s
            .parse::<WorkUnitId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        required_artifact_ids: parse_vec(&artifacts_s)?,
        required_state,
        checklist: parse_vec(&checklist_s)?,
        owner_agent_id: owner_s
            .map(|s| s.parse::<daruma_shared::AgentId>())
            .transpose()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        accepted_by_agent_id: accepted_s
            .map(|s| s.parse::<daruma_shared::AgentId>())
            .transpose()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        status,
        notes,
        required_changes: parse_vec(&changes_s)?,
        created_at: crate::parse_ts(&created_at_s)?,
        updated_at: crate::parse_ts(&updated_at_s)?,
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_domain::{Actor, NewHandoffContract};
    use daruma_shared::time;

    async fn repo() -> HandoffRepo {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        HandoffRepo::new(db.pool().clone())
    }

    fn contract(from: WorkUnitId, to: WorkUnitId) -> HandoffContract {
        NewHandoffContract {
            from_work_unit_id: from,
            to_work_unit_id: to,
            required_artifact_ids: vec!["artifact://api/dashboard@v1".into()],
            required_state: Some("approved".into()),
            checklist: vec!["contract published".into()],
            owner_agent_id: None,
        }
        .into_contract(HandoffId::new(), time::now())
    }

    async fn apply(repo: &HandoffRepo, ev: Event) {
        let env = EventEnvelope::new(Actor::user(), ev);
        repo.apply_event(&env).await.unwrap();
    }

    #[tokio::test]
    async fn request_accept_roundtrip() {
        let repo = repo().await;
        let (from, to) = (WorkUnitId::new(), WorkUnitId::new());
        let c = contract(from, to);
        let id = c.id;
        apply(&repo, Event::HandoffRequested { handoff: c }).await;

        let fetched = repo.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.status, HandoffStatus::Open);
        assert_eq!(fetched.required_artifact_ids.len(), 1);

        apply(
            &repo,
            Event::HandoffAccepted {
                handoff_id: id,
                by: None,
                notes: Some("looks complete".into()),
                at: time::now(),
            },
        )
        .await;
        let fetched = repo.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.status, HandoffStatus::Accepted);
        assert_eq!(fetched.notes.as_deref(), Some("looks complete"));
    }

    #[tokio::test]
    async fn reject_then_rerequest_reopens_same_row() {
        let repo = repo().await;
        let (from, to) = (WorkUnitId::new(), WorkUnitId::new());
        let c = contract(from, to);
        let id = c.id;
        apply(&repo, Event::HandoffRequested { handoff: c.clone() }).await;
        apply(
            &repo,
            Event::HandoffRejected {
                handoff_id: id,
                reason: "missing error cases".into(),
                required_changes: vec!["add 4xx handling".into()],
                at: time::now(),
            },
        )
        .await;
        let fetched = repo.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.status, HandoffStatus::Rejected);
        assert_eq!(fetched.required_changes, vec!["add 4xx handling"]);

        // Re-request with the same id: row reopens, rejection detail clears.
        let mut revised = c;
        revised.checklist.push("4xx handling covered".into());
        apply(&repo, Event::HandoffRequested { handoff: revised }).await;
        let fetched = repo.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.status, HandoffStatus::Open);
        assert_eq!(fetched.checklist.len(), 2);
        assert!(fetched.required_changes.is_empty());

        // Pair lookup sees exactly this row.
        let by_pair = repo.get_by_pair(from, to).await.unwrap().unwrap();
        assert_eq!(by_pair.id, id);
        assert_eq!(repo.list_for_work_unit(to).await.unwrap().len(), 1);
    }
}
