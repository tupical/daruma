//! Evidence registry repository — projection over `evidence` (migration 0038),
//! fed by `EvidenceRecorded` / `EvidenceSuperseded` events. The event log stays
//! the source of truth (spec invariant 6).
//!
//! Reads serve two callers: listing (HTTP/MCP) and the lifecycle gate, which
//! asks whether *live* (non-superseded) evidence of a given kind exists for a
//! scope chain — the carrier that lets a `required` rule pass (spec §1.3).
//!
//! Immutability: rows are inserted and (on supersede) marked, never updated in
//! place except to set `superseded_by`.

use sqlx::{Row, SqlitePool};
use taskagent_domain::{ActorRef, Evidence, EvidenceKind, RuleScope};
use taskagent_events::{Event, EventEnvelope};
use taskagent_shared::{CoreError, EvidenceId, Result};

pub struct EvidenceRepo {
    pool: SqlitePool,
}

impl EvidenceRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── queries ────────────────────────────────────────────────────────────────

    pub async fn get(&self, id: EvidenceId) -> Result<Option<Evidence>> {
        let row = sqlx::query(&select_sql("WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        row.as_ref().map(row_to_evidence).transpose()
    }

    /// Evidence recorded directly at a scope level (newest first), for listing.
    /// `include_superseded=false` hides retracted records.
    pub async fn list_for_scope(
        &self,
        scope: &RuleScope,
        include_superseded: bool,
    ) -> Result<Vec<Evidence>> {
        let live = if include_superseded {
            ""
        } else {
            " AND superseded_by IS NULL"
        };
        let rows = match scope.id_string() {
            Some(id) => {
                sqlx::query(&select_sql(&format!(
                    "WHERE scope_kind = ? AND scope_id = ?{live} ORDER BY recorded_at DESC, id DESC"
                )))
                .bind(scope.kind())
                .bind(id)
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query(&select_sql(&format!(
                    "WHERE scope_kind = ? AND scope_id IS NULL{live} \
                     ORDER BY recorded_at DESC, id DESC"
                )))
                .bind(scope.kind())
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.iter().map(row_to_evidence).collect()
    }

    /// Gate hot path: does *live* evidence of `kind` exist anywhere in the scope
    /// chain, optionally matching `target`? `target = None` accepts any target;
    /// `target = Some(t)` accepts a record whose `target` is `t` *or* `NULL`
    /// (untargeted evidence satisfies a targeted requirement — the broader proof
    /// covers the narrower ask). Returns on the first match; one indexed query
    /// per scope, short-circuiting, so it stays cheap and deterministic.
    pub async fn has_live_evidence(
        &self,
        chain: &[RuleScope],
        kind: EvidenceKind,
        target: Option<&str>,
    ) -> Result<bool> {
        for scope in chain {
            let matched = match scope.id_string() {
                Some(id) => self.scope_has(scope.kind(), Some(id), kind, target).await?,
                None => self.scope_has(scope.kind(), None, kind, target).await?,
            };
            if matched {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn scope_has(
        &self,
        scope_kind: &str,
        scope_id: Option<String>,
        kind: EvidenceKind,
        target: Option<&str>,
    ) -> Result<bool> {
        // `target` filter: when the requirement names a target, accept evidence
        // that names the same target OR no target at all.
        let target_clause = match target {
            Some(_) => "AND (target = ? OR target IS NULL)",
            None => "",
        };
        let scope_clause = if scope_id.is_some() {
            "scope_id = ?"
        } else {
            "scope_id IS NULL"
        };
        let sql = format!(
            "SELECT 1 FROM evidence \
             WHERE scope_kind = ? AND {scope_clause} AND kind = ? \
             AND superseded_by IS NULL {target_clause} LIMIT 1"
        );
        let mut q = sqlx::query(&sql).bind(scope_kind);
        if let Some(id) = scope_id {
            q = q.bind(id);
        }
        q = q.bind(kind.as_str());
        if let Some(t) = target {
            q = q.bind(t);
        }
        let row = q
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(row.is_some())
    }

    /// Apply a persisted evidence event to the projection.
    pub async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        match &env.payload {
            Event::EvidenceRecorded { evidence } => self.insert(evidence).await,
            Event::EvidenceSuperseded {
                evidence_id,
                superseded_by,
                ..
            } => {
                sqlx::query("UPDATE evidence SET superseded_by = ? WHERE id = ?")
                    .bind(superseded_by.to_string())
                    .bind(evidence_id.to_string())
                    .execute(&self.pool)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Insert is idempotent on `id` (replay-safe). Immutable: an existing row is
    /// left untouched on conflict.
    async fn insert(&self, ev: &Evidence) -> Result<()> {
        let payload =
            serde_json::to_string(&ev.payload).map_err(|e| CoreError::serde(e.to_string()))?;
        sqlx::query(
            "INSERT INTO evidence \
             (id, kind, scope_kind, scope_id, target, doc_version, \
              actor_kind, actor_id, actor_name, reason, payload, \
              project_id, plan_id, task_id, run_id, artifact_id, rule_id, \
              recorded_at, superseded_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(ev.id.to_string())
        .bind(ev.kind.as_str())
        .bind(ev.scope.kind())
        .bind(ev.scope.id_string())
        .bind(ev.target.as_deref())
        .bind(ev.doc_version.as_deref())
        .bind(&ev.actor.kind)
        .bind(ev.actor.id.map(|i| i.to_string()))
        .bind(ev.actor.name.as_deref())
        .bind(&ev.reason)
        .bind(payload)
        .bind(ev.project_id.map(|i| i.to_string()))
        .bind(ev.plan_id.map(|i| i.to_string()))
        .bind(ev.task_id.map(|i| i.to_string()))
        .bind(ev.run_id.map(|i| i.to_string()))
        .bind(ev.artifact_id.map(|i| i.to_string()))
        .bind(ev.rule_id.map(|i| i.to_string()))
        .bind(ev.recorded_at.to_rfc3339())
        .bind(ev.superseded_by.map(|i| i.to_string()))
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }
}

fn select_sql(tail: &str) -> String {
    format!(
        "SELECT id, kind, scope_kind, scope_id, target, doc_version, \
         actor_kind, actor_id, actor_name, reason, payload, \
         project_id, plan_id, task_id, run_id, artifact_id, rule_id, \
         recorded_at, superseded_by \
         FROM evidence {tail}"
    )
}

fn row_to_evidence(row: &sqlx::sqlite::SqliteRow) -> Result<Evidence> {
    let id_str: String = row.try_get("id").map_err(map_row_err)?;
    let kind_str: String = row.try_get("kind").map_err(map_row_err)?;
    let scope_kind: String = row.try_get("scope_kind").map_err(map_row_err)?;
    let scope_id: Option<String> = row.try_get("scope_id").map_err(map_row_err)?;
    let payload_json: String = row.try_get("payload").map_err(map_row_err)?;
    let recorded_at: String = row.try_get("recorded_at").map_err(map_row_err)?;
    let actor_id: Option<String> = row.try_get("actor_id").map_err(map_row_err)?;
    let superseded_by: Option<String> = row.try_get("superseded_by").map_err(map_row_err)?;

    let kind = EvidenceKind::parse_str(&kind_str)
        .ok_or_else(|| CoreError::storage(format!("unknown evidence kind: {kind_str}")))?;
    let scope = parse_scope(&scope_kind, scope_id.as_deref())?;
    let payload: serde_json::Value =
        serde_json::from_str(&payload_json).map_err(|e| CoreError::serde(e.to_string()))?;

    Ok(Evidence {
        id: id_str
            .parse()
            .map_err(|_| CoreError::storage("bad evidence id"))?,
        kind,
        scope,
        target: row.try_get("target").map_err(map_row_err)?,
        doc_version: row.try_get("doc_version").map_err(map_row_err)?,
        actor: ActorRef {
            kind: row.try_get("actor_kind").map_err(map_row_err)?,
            id: actor_id
                .map(|s| s.parse())
                .transpose()
                .map_err(|_| CoreError::storage("bad actor id"))?,
            name: row.try_get("actor_name").map_err(map_row_err)?,
        },
        reason: row.try_get("reason").map_err(map_row_err)?,
        payload,
        project_id: parse_opt_id(row, "project_id")?,
        plan_id: parse_opt_id(row, "plan_id")?,
        task_id: parse_opt_id(row, "task_id")?,
        run_id: parse_opt_id(row, "run_id")?,
        artifact_id: parse_opt_id(row, "artifact_id")?,
        rule_id: parse_opt_id(row, "rule_id")?,
        recorded_at: parse_ts(&recorded_at)?,
        superseded_by: superseded_by
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| CoreError::storage("bad superseded_by id"))?,
    })
}

fn parse_opt_id<T: std::str::FromStr>(
    row: &sqlx::sqlite::SqliteRow,
    col: &str,
) -> Result<Option<T>> {
    let raw: Option<String> = row.try_get(col).map_err(map_row_err)?;
    raw.map(|s| s.parse())
        .transpose()
        .map_err(|_| CoreError::storage(format!("bad {col}")))
}

fn map_row_err(e: sqlx::Error) -> CoreError {
    CoreError::storage(e.to_string())
}

fn parse_ts(s: &str) -> Result<taskagent_shared::Timestamp> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| CoreError::storage(e.to_string()))
}

fn parse_scope(kind: &str, id: Option<&str>) -> Result<RuleScope> {
    Ok(match kind {
        "tenant" => RuleScope::Tenant,
        "project" => RuleScope::Project {
            id: scope_id(id, "project")?,
        },
        "plan" => RuleScope::Plan {
            id: scope_id(id, "plan")?,
        },
        "task" => RuleScope::Task {
            id: scope_id(id, "task")?,
        },
        other => {
            return Err(CoreError::storage(format!(
                "unknown evidence scope kind: {other}"
            )))
        }
    })
}

fn scope_id<T: std::str::FromStr>(id: Option<&str>, kind: &str) -> Result<T> {
    id.ok_or_else(|| CoreError::storage(format!("{kind} scope missing scope_id")))?
        .parse()
        .map_err(|_| CoreError::storage(format!("bad {kind} scope id")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use taskagent_domain::{Actor, NewEvidence};
    use taskagent_shared::ProjectId;

    fn sample(scope: RuleScope, kind: EvidenceKind, target: Option<&str>) -> Evidence {
        NewEvidence {
            id: None,
            kind,
            scope,
            target: target.map(|s| s.to_string()),
            doc_version: None,
            reason: "r".into(),
            payload: serde_json::Value::Null,
            project_id: None,
            plan_id: None,
            task_id: None,
            run_id: None,
            artifact_id: None,
            rule_id: None,
            supersedes: None,
        }
        .into_evidence(
            ActorRef::from_actor(&Actor::User),
            taskagent_shared::time::now(),
        )
    }

    async fn repo() -> EvidenceRepo {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        EvidenceRepo::new(db.pool().clone())
    }

    async fn apply(repo: &EvidenceRepo, ev: Event) {
        let env = EventEnvelope::new(Actor::user(), ev);
        repo.apply_event(&env).await.unwrap();
    }

    #[tokio::test]
    async fn record_get_roundtrip() {
        let repo = repo().await;
        let ev = sample(RuleScope::Tenant, EvidenceKind::CompletionNote, None);
        let id = ev.id;
        apply(&repo, Event::EvidenceRecorded { evidence: ev }).await;

        let fetched = repo.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.kind, EvidenceKind::CompletionNote);
        assert!(fetched.superseded_by.is_none());
    }

    #[tokio::test]
    async fn has_live_evidence_matches_in_chain() {
        let repo = repo().await;
        let project = ProjectId::new();
        // Recorded at tenant; a project-scoped check walks tenant in its chain.
        apply(
            &repo,
            Event::EvidenceRecorded {
                evidence: sample(RuleScope::Tenant, EvidenceKind::CompletionNote, None),
            },
        )
        .await;
        let chain = [RuleScope::Tenant, RuleScope::Project { id: project }];
        assert!(repo
            .has_live_evidence(&chain, EvidenceKind::CompletionNote, None)
            .await
            .unwrap());
        // Wrong kind → no match.
        assert!(!repo
            .has_live_evidence(&chain, EvidenceKind::RiskCheckCompleted, None)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn superseded_evidence_is_not_live() {
        let repo = repo().await;
        let ev = sample(RuleScope::Tenant, EvidenceKind::ImpactAssessment, None);
        let id = ev.id;
        apply(&repo, Event::EvidenceRecorded { evidence: ev }).await;
        let newer = sample(RuleScope::Tenant, EvidenceKind::ImpactAssessment, None);
        let newer_id = newer.id;
        apply(&repo, Event::EvidenceRecorded { evidence: newer }).await;
        apply(
            &repo,
            Event::EvidenceSuperseded {
                evidence_id: id,
                superseded_by: newer_id,
                at: taskagent_shared::time::now(),
            },
        )
        .await;

        // Old row marked, newer still live → chain still satisfied.
        assert_eq!(
            repo.get(id).await.unwrap().unwrap().superseded_by,
            Some(newer_id)
        );
        assert!(repo
            .has_live_evidence(&[RuleScope::Tenant], EvidenceKind::ImpactAssessment, None)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn targeted_requirement_accepts_matching_or_untargeted() {
        let repo = repo().await;
        apply(
            &repo,
            Event::EvidenceRecorded {
                evidence: sample(
                    RuleScope::Tenant,
                    EvidenceKind::DocumentReadAck,
                    Some("architecture.md"),
                ),
            },
        )
        .await;
        let chain = [RuleScope::Tenant];
        // Exact target match.
        assert!(repo
            .has_live_evidence(
                &chain,
                EvidenceKind::DocumentReadAck,
                Some("architecture.md")
            )
            .await
            .unwrap());
        // Different target → not satisfied.
        assert!(!repo
            .has_live_evidence(&chain, EvidenceKind::DocumentReadAck, Some("other.md"))
            .await
            .unwrap());
    }
}
