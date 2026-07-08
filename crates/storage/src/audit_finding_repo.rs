//! Audit findings repository — the store behind `audit_findings` (migration
//! 0041, Audit primitives task B). Unlike the evidence registry, findings are
//! *not* event-sourced and *not* immutable: a check upserts on its dedup key and
//! auto-resolves findings it no longer reproduces. The table is the source of
//! truth; there is no projection to rebuild.
//!
//! Two write entry points map onto the two invariants:
//!
//! - [`AuditFindingRepo::upsert`] — record one sighting. A fresh finding opens;
//!   a repeat sighting (same `(project, check_key, entity)`) bumps `last_seen_at`
//!   and refreshes the mutable fields, never inserting a duplicate.
//! - [`AuditFindingRepo::resolve_missing`] — after a check run, resolve every
//!   still-open finding of a `check_key` in a project that was *not* seen this
//!   run (auto-resolve). The caller passes the ids it upserted.

use crate::parse_ts;
use daruma_domain::{
    ActorRef, AuditFinding, FindingEntity, FindingSeverity, FindingSource, FindingStatus,
    NewFinding,
};
use daruma_shared::{time, AuditFindingId, CoreError, Result, Timestamp};
use sqlx::{Row, SqlitePool};

/// Filters for [`AuditFindingRepo::list`]. All `None` = every finding in the
/// project; combine to narrow.
#[derive(Clone, Debug, Default)]
pub struct FindingFilter {
    pub severity: Option<FindingSeverity>,
    pub category: Option<String>,
    pub status: Option<FindingStatus>,
}

pub struct AuditFindingRepo {
    pool: SqlitePool,
}

impl AuditFindingRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── writes ─────────────────────────────────────────────────────────────────

    /// Record a sighting of a finding, idempotent on `(project_id, check_key,
    /// entity tuple)`. On first sight a new row opens (`first_seen_at =
    /// last_seen_at = now`, status `Open`); on a repeat sight the existing row's
    /// `last_seen_at` and the mutable descriptive fields are refreshed and the
    /// row is re-opened if it had auto-resolved (the problem came back). Returns
    /// the row's id either way.
    pub async fn upsert(&self, new: &NewFinding) -> Result<AuditFindingId> {
        let now = time::now();
        // Look up the existing row on the dedup key (NULLs coalesced to '' to
        // match migration 0041's unique index).
        let existing: Option<(String, String)> = sqlx::query_as(
            "SELECT id, status FROM audit_findings \
             WHERE project_id = ? AND check_key = ? \
               AND COALESCE(plan_id, '') = ? AND COALESCE(task_id, '') = ? \
               AND COALESCE(document_id, '') = ? AND COALESCE(artifact_id, '') = ?",
        )
        .bind(new.project_id.to_string())
        .bind(&new.check_key)
        .bind(entity_key(&new.entity.plan_id))
        .bind(entity_key(&new.entity.task_id))
        .bind(entity_key(&new.entity.document_id))
        .bind(entity_key(&new.entity.artifact_id))
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        if let Some((id_str, _status)) = existing {
            // Repeat sighting: refresh mutable fields, bump last_seen, and
            // re-open (clear any prior auto-resolve — the problem is back).
            sqlx::query(
                "UPDATE audit_findings \
                 SET category = ?, severity = ?, title = ?, detail = ?, remediation = ?, \
                     source = ?, status = 'open', last_seen_at = ?, \
                     resolved_by = NULL, resolved_at = NULL \
                 WHERE id = ?",
            )
            .bind(&new.category)
            .bind(new.severity.as_str())
            .bind(&new.title)
            .bind(&new.detail)
            .bind(&new.remediation)
            .bind(new.source.as_str())
            .bind(now.to_rfc3339())
            .bind(&id_str)
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
            return id_str
                .parse()
                .map_err(|_| CoreError::storage("bad audit finding id"));
        }

        // First sighting: open a new finding.
        let id = AuditFindingId::new();
        sqlx::query(
            "INSERT INTO audit_findings \
             (id, project_id, plan_id, task_id, document_id, artifact_id, \
              check_key, category, severity, title, detail, remediation, source, status, \
              first_seen_at, last_seen_at, resolved_by, resolved_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'open', ?, ?, NULL, NULL)",
        )
        .bind(id.to_string())
        .bind(new.project_id.to_string())
        .bind(new.entity.plan_id.map(|i| i.to_string()))
        .bind(new.entity.task_id.map(|i| i.to_string()))
        .bind(new.entity.document_id.map(|i| i.to_string()))
        .bind(new.entity.artifact_id.map(|i| i.to_string()))
        .bind(&new.check_key)
        .bind(&new.category)
        .bind(new.severity.as_str())
        .bind(&new.title)
        .bind(&new.detail)
        .bind(&new.remediation)
        .bind(new.source.as_str())
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(id)
    }

    /// Auto-resolve: after a full check run, flip every still-open finding of
    /// `check_key` in `project_id` that was *not* re-seen this run (its id is not
    /// in `seen`) to `Resolved`. `resolved_by` records who ran the check. Returns
    /// the number of findings resolved. With an empty `seen` slice every open
    /// finding of the key resolves (the check found nothing this run).
    pub async fn resolve_missing(
        &self,
        project_id: daruma_shared::ProjectId,
        check_key: &str,
        seen: &[AuditFindingId],
        resolved_by: &ActorRef,
        now: Timestamp,
    ) -> Result<u64> {
        // Build a NOT IN (?, ?, …) exclusion. SQLite has no array binding, so we
        // assemble placeholders for the (typically small) seen set.
        let placeholders = if seen.is_empty() {
            String::new()
        } else {
            let marks = vec!["?"; seen.len()].join(", ");
            format!("AND id NOT IN ({marks})")
        };
        let sql = format!(
            "UPDATE audit_findings \
             SET status = 'resolved', resolved_by = ?, resolved_at = ? \
             WHERE project_id = ? AND check_key = ? AND status != 'resolved' {placeholders}"
        );
        let mut q = sqlx::query(&sql)
            .bind(resolved_by.kind.clone())
            .bind(now.to_rfc3339())
            .bind(project_id.to_string())
            .bind(check_key);
        for id in seen {
            q = q.bind(id.to_string());
        }
        let res = q
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(res.rows_affected())
    }

    /// Set the status of a finding (operator action: acknowledge / mute /
    /// resolve / re-open). `resolved_by` / `resolved_at` are set when moving to
    /// `Resolved`, cleared otherwise. Returns `false` if the id is unknown.
    pub async fn set_status(
        &self,
        id: AuditFindingId,
        status: FindingStatus,
        actor: &ActorRef,
        now: Timestamp,
    ) -> Result<bool> {
        let res = if status == FindingStatus::Resolved {
            sqlx::query(
                "UPDATE audit_findings SET status = ?, resolved_by = ?, resolved_at = ? \
                 WHERE id = ?",
            )
            .bind(status.as_str())
            .bind(actor.kind.clone())
            .bind(now.to_rfc3339())
            .bind(id.to_string())
            .execute(&self.pool)
            .await
        } else {
            sqlx::query(
                "UPDATE audit_findings SET status = ?, resolved_by = NULL, resolved_at = NULL \
                 WHERE id = ?",
            )
            .bind(status.as_str())
            .bind(id.to_string())
            .execute(&self.pool)
            .await
        }
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(res.rows_affected() > 0)
    }

    // ── reads ──────────────────────────────────────────────────────────────────

    pub async fn get(&self, id: AuditFindingId) -> Result<Option<AuditFinding>> {
        let row = sqlx::query(&select_sql("WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        row.as_ref().map(row_to_finding).transpose()
    }

    /// List findings in a project, newest activity first, narrowed by `filter`.
    /// The query is built branch-free with always-true `(? IS NULL OR col = ?)`
    /// guards so SQLite gets one stable prepared statement.
    pub async fn list(
        &self,
        project_id: daruma_shared::ProjectId,
        filter: &FindingFilter,
    ) -> Result<Vec<AuditFinding>> {
        let rows = sqlx::query(&select_sql(
            "WHERE project_id = ? \
               AND (? IS NULL OR severity = ?) \
               AND (? IS NULL OR category = ?) \
               AND (? IS NULL OR status = ?) \
             ORDER BY last_seen_at DESC, id DESC",
        ))
        .bind(project_id.to_string())
        .bind(filter.severity.map(|s| s.as_str()))
        .bind(filter.severity.map(|s| s.as_str()))
        .bind(filter.category.as_deref())
        .bind(filter.category.as_deref())
        .bind(filter.status.map(|s| s.as_str()))
        .bind(filter.status.map(|s| s.as_str()))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.iter().map(row_to_finding).collect()
    }
}

/// Coalesce an optional id into the dedup-key string ('' for `None`), matching
/// migration 0041's `COALESCE(col, '')` unique index.
fn entity_key<T: std::fmt::Display>(id: &Option<T>) -> String {
    id.as_ref().map(|i| i.to_string()).unwrap_or_default()
}

fn select_sql(tail: &str) -> String {
    format!(
        "SELECT id, project_id, plan_id, task_id, document_id, artifact_id, \
         check_key, category, severity, title, detail, remediation, source, status, \
         first_seen_at, last_seen_at, resolved_by, resolved_at \
         FROM audit_findings {tail}"
    )
}

fn row_to_finding(row: &sqlx::sqlite::SqliteRow) -> Result<AuditFinding> {
    let severity_str: String = row.try_get("severity").map_err(map_err)?;
    let status_str: String = row.try_get("status").map_err(map_err)?;
    let source_str: String = row.try_get("source").map_err(map_err)?;
    let resolved_by: Option<String> = row.try_get("resolved_by").map_err(map_err)?;
    let resolved_at: Option<String> = row.try_get("resolved_at").map_err(map_err)?;

    Ok(AuditFinding {
        id: parse_id(row, "id")?,
        project_id: parse_id(row, "project_id")?,
        entity: FindingEntity {
            plan_id: parse_opt_id(row, "plan_id")?,
            task_id: parse_opt_id(row, "task_id")?,
            document_id: parse_opt_id(row, "document_id")?,
            artifact_id: parse_opt_id(row, "artifact_id")?,
        },
        check_key: row.try_get("check_key").map_err(map_err)?,
        category: row.try_get("category").map_err(map_err)?,
        severity: FindingSeverity::parse_str(&severity_str).ok_or_else(|| {
            CoreError::storage(format!("unknown finding severity: {severity_str}"))
        })?,
        title: row.try_get("title").map_err(map_err)?,
        detail: row.try_get("detail").map_err(map_err)?,
        remediation: row.try_get("remediation").map_err(map_err)?,
        source: FindingSource::parse_str(&source_str)
            .ok_or_else(|| CoreError::storage(format!("unknown finding source: {source_str}")))?,
        status: FindingStatus::parse_str(&status_str)
            .ok_or_else(|| CoreError::storage(format!("unknown finding status: {status_str}")))?,
        first_seen_at: parse_ts(&row.try_get::<String, _>("first_seen_at").map_err(map_err)?)?,
        last_seen_at: parse_ts(&row.try_get::<String, _>("last_seen_at").map_err(map_err)?)?,
        // `resolved_by` is stored as a flat actor-kind string; surface it as an
        // `ActorRef` with just the kind populated (parity with how the column is
        // written). The full actor triple is not retained for findings.
        resolved_by: resolved_by.map(|kind| ActorRef {
            kind,
            id: None,
            name: None,
        }),
        resolved_at: resolved_at.as_deref().map(parse_ts).transpose()?,
    })
}

fn parse_id<T: std::str::FromStr>(row: &sqlx::sqlite::SqliteRow, col: &str) -> Result<T> {
    let s: String = row.try_get(col).map_err(map_err)?;
    s.parse()
        .map_err(|_| CoreError::storage(format!("bad {col}")))
}

fn parse_opt_id<T: std::str::FromStr>(
    row: &sqlx::sqlite::SqliteRow,
    col: &str,
) -> Result<Option<T>> {
    let raw: Option<String> = row.try_get(col).map_err(map_err)?;
    raw.filter(|s| !s.is_empty())
        .map(|s| s.parse())
        .transpose()
        .map_err(|_| CoreError::storage(format!("bad {col}")))
}

fn map_err(e: sqlx::Error) -> CoreError {
    CoreError::storage(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_domain::Actor;
    use daruma_shared::{ProjectId, TaskId};

    async fn repo() -> AuditFindingRepo {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        AuditFindingRepo::new(db.pool().clone())
    }

    fn sample(project: ProjectId, task: Option<TaskId>, check: &str) -> NewFinding {
        NewFinding {
            project_id: project,
            entity: FindingEntity {
                task_id: task,
                ..Default::default()
            },
            check_key: check.into(),
            category: "staleness".into(),
            severity: FindingSeverity::Warn,
            title: "stuck".into(),
            detail: "detail".into(),
            remediation: "do x".into(),
            source: FindingSource::Script,
        }
    }

    #[tokio::test]
    async fn upsert_is_idempotent_on_dedup_key() {
        let repo = repo().await;
        let project = ProjectId::new();
        let task = TaskId::new();
        let f = sample(project, Some(task), "task.stuck");

        let id1 = repo.upsert(&f).await.unwrap();
        // Re-run the same check: same row, last_seen bumped, no duplicate.
        let id2 = repo.upsert(&f).await.unwrap();
        assert_eq!(id1, id2, "repeat sighting must reuse the row");

        let all = repo.list(project, &FindingFilter::default()).await.unwrap();
        assert_eq!(all.len(), 1, "no duplicate finding");
        let got = repo.get(id1).await.unwrap().unwrap();
        assert!(got.last_seen_at >= got.first_seen_at);
        assert_eq!(got.status, FindingStatus::Open);
    }

    #[tokio::test]
    async fn distinct_entities_are_distinct_findings() {
        let repo = repo().await;
        let project = ProjectId::new();
        let a = repo
            .upsert(&sample(project, Some(TaskId::new()), "task.stuck"))
            .await
            .unwrap();
        let b = repo
            .upsert(&sample(project, Some(TaskId::new()), "task.stuck"))
            .await
            .unwrap();
        assert_ne!(a, b);
        assert_eq!(
            repo.list(project, &FindingFilter::default())
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn resolve_missing_auto_resolves_unseen() {
        let repo = repo().await;
        let project = ProjectId::new();
        let still_here = repo
            .upsert(&sample(project, Some(TaskId::new()), "task.stuck"))
            .await
            .unwrap();
        let gone = repo
            .upsert(&sample(project, Some(TaskId::new()), "task.stuck"))
            .await
            .unwrap();

        // Next run only re-saw `still_here`; `gone` should auto-resolve.
        let resolved = repo
            .resolve_missing(
                project,
                "task.stuck",
                &[still_here],
                &ActorRef::from_actor(&Actor::User),
                time::now(),
            )
            .await
            .unwrap();
        assert_eq!(resolved, 1);
        assert_eq!(
            repo.get(still_here).await.unwrap().unwrap().status,
            FindingStatus::Open
        );
        let gone_row = repo.get(gone).await.unwrap().unwrap();
        assert_eq!(gone_row.status, FindingStatus::Resolved);
        assert!(gone_row.resolved_at.is_some());
    }

    #[tokio::test]
    async fn reupsert_reopens_resolved_finding() {
        let repo = repo().await;
        let project = ProjectId::new();
        let task = TaskId::new();
        let id = repo
            .upsert(&sample(project, Some(task), "task.stuck"))
            .await
            .unwrap();
        repo.resolve_missing(
            project,
            "task.stuck",
            &[],
            &ActorRef::from_actor(&Actor::User),
            time::now(),
        )
        .await
        .unwrap();
        assert_eq!(
            repo.get(id).await.unwrap().unwrap().status,
            FindingStatus::Resolved
        );

        // Problem comes back → upsert re-opens the same row.
        let id2 = repo
            .upsert(&sample(project, Some(task), "task.stuck"))
            .await
            .unwrap();
        assert_eq!(id, id2);
        let row = repo.get(id).await.unwrap().unwrap();
        assert_eq!(row.status, FindingStatus::Open);
        assert!(row.resolved_at.is_none());
    }

    #[tokio::test]
    async fn list_filters_by_severity_and_status() {
        let repo = repo().await;
        let project = ProjectId::new();
        let mut warn = sample(project, Some(TaskId::new()), "a");
        warn.severity = FindingSeverity::Warn;
        let mut err = sample(project, Some(TaskId::new()), "b");
        err.severity = FindingSeverity::Error;
        repo.upsert(&warn).await.unwrap();
        repo.upsert(&err).await.unwrap();

        let errors = repo
            .list(
                project,
                &FindingFilter {
                    severity: Some(FindingSeverity::Error),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].severity, FindingSeverity::Error);
    }

    #[tokio::test]
    async fn set_status_acknowledge_and_resolve() {
        let repo = repo().await;
        let project = ProjectId::new();
        let id = repo
            .upsert(&sample(project, Some(TaskId::new()), "a"))
            .await
            .unwrap();
        let actor = ActorRef::from_actor(&Actor::User);

        assert!(repo
            .set_status(id, FindingStatus::Acknowledged, &actor, time::now())
            .await
            .unwrap());
        assert_eq!(
            repo.get(id).await.unwrap().unwrap().status,
            FindingStatus::Acknowledged
        );

        assert!(repo
            .set_status(id, FindingStatus::Resolved, &actor, time::now())
            .await
            .unwrap());
        let row = repo.get(id).await.unwrap().unwrap();
        assert_eq!(row.status, FindingStatus::Resolved);
        assert!(row.resolved_at.is_some());

        // Unknown id → false.
        assert!(!repo
            .set_status(
                AuditFindingId::new(),
                FindingStatus::Muted,
                &actor,
                time::now()
            )
            .await
            .unwrap());
    }
}
