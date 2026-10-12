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

use crate::parse_ts;
use daruma_domain::{ActorRef, Evidence, EvidenceKind, EvidenceReach, RuleScope};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{CoreError, EvidenceId, Result};
use sqlx::{Row, SqlitePool};

pub struct EvidenceRepo {
    pool: SqlitePool,
}

/// Result of matching live evidence, including why candidates were rejected.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EvidenceCheck {
    pub satisfied: bool,
    pub reason: Option<String>,
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

    /// Test attestations are task-local and revision-bound; payload actor labels are untrusted.
    pub async fn has_independent_test_verification(
        &self,
        task_id: daruma_shared::TaskId,
        executor_id: daruma_shared::AgentId,
        source_revision: &str,
    ) -> Result<bool> {
        let found: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM evidence WHERE scope_kind = 'task' AND scope_id = ? \
             AND kind = 'artifact_created' AND target = ? AND superseded_by IS NULL \
             AND authenticated_actor_id IS NOT NULL AND authenticated_actor_id = actor_id AND actor_id != ? \
             AND json_type(payload, '$.passed') = 'true')",
        )
        .bind(task_id.to_string())
        .bind(format!("test-verification:{source_revision}"))
        .bind(executor_id.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(found != 0)
    }

    /// `document_linked` requirement: does `task_id` have at least one bound,
    /// non-archived document? Reads the `documents` projection directly — the
    /// binding is the proof, no evidence row exists for it. `archived_at` is
    /// checked too: rows archived before migration 0042 keep `status='active'`.
    pub async fn task_has_live_document(&self, task_id: daruma_shared::TaskId) -> Result<bool> {
        let found: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM documents WHERE task_id = ? \
             AND status != 'archived' AND archived_at IS NULL)",
        )
        .bind(task_id.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(found != 0)
    }

    /// Gate hot path: does *live* evidence of `kind` exist anywhere in the scope
    /// chain the kind can legitimately reach, optionally matching `target`?
    ///
    /// `target = None` accepts any target; `target = Some(t)` accepts a record
    /// whose `target` is `t` *or* `NULL` (untargeted evidence satisfies a
    /// targeted requirement — the broader proof covers the narrower ask).
    ///
    /// The two axes are not symmetric, and the difference is the whole point of
    /// [`EvidenceKind::reach`]. `target` enumerates referents that already
    /// exist, so a broader proof there really is a stronger claim about the same
    /// set. `scope` quantifies over an open set that includes entities not yet
    /// created — so a tenant-wide "acceptance criteria are defined" is a claim
    /// about tasks nobody has written, which is policy wearing the costume of
    /// proof. Hence: the broader proof covers the narrower ask *within the
    /// kind's semantic reach*, and no further.
    ///
    /// Up to 20 live candidates per scope are checked for required non-empty
    /// fields and the document version. A numeric `min_version` is a minimum;
    /// `latest` with a `target` naming a versioned document demands an ack of
    /// exactly its current `entity_versions` number, targeted at that document
    /// (spec §1.3: a stale or untargeted read does not count). `latest` for a
    /// target with no recorded versions keeps the legacy any-version match.
    /// Required fields are read
    /// from `payload` first; only when a key is absent there, `reason`, `actor`,
    /// `doc_version`, and `target` fall back to their top-level columns. The
    /// first valid candidate satisfies the requirement; otherwise the rejection
    /// reason is returned for the gate decision.
    pub async fn has_live_evidence(
        &self,
        chain: &[RuleScope],
        kind: EvidenceKind,
        target: Option<&str>,
        required_fields: Option<&[String]>,
        min_version: Option<&str>,
    ) -> Result<EvidenceCheck> {
        let version_rule = match min_version {
            None => VersionRule::Any,
            Some("latest") => match target {
                Some(doc_ref) => match self.current_document_version(doc_ref).await? {
                    Some(current) => VersionRule::Latest(current),
                    None => VersionRule::Any,
                },
                None => VersionRule::Any,
            },
            Some(version) => match parse_version(version) {
                Some(minimum) => VersionRule::AtLeast(minimum),
                None => {
                    return Ok(EvidenceCheck {
                        satisfied: false,
                        reason: Some(format!(
                            "rule min_version `{version}` is invalid; expected an integer with optional v/V prefix"
                        )),
                    })
                }
            },
        };
        let mut reason = None;
        for scope in reachable(chain, kind.reach()) {
            let check = self
                .scope_has(
                    scope.kind(),
                    scope.id_string(),
                    kind,
                    target,
                    required_fields,
                    version_rule,
                )
                .await?;
            if check.satisfied {
                return Ok(check);
            }
            if reason.is_none() {
                reason = check.reason;
            }
        }
        if let (None, VersionRule::Latest(current), Some(doc_ref)) = (&reason, version_rule, target)
        {
            reason = Some(format!(
                "requires a document_read_ack targeting `{doc_ref}` with doc_version {current} (current document version, min_version=latest)"
            ));
        }
        Ok(EvidenceCheck {
            satisfied: false,
            reason,
        })
    }

    async fn scope_has(
        &self,
        scope_kind: &str,
        scope_id: Option<String>,
        kind: EvidenceKind,
        target: Option<&str>,
        required_fields: Option<&[String]>,
        version_rule: VersionRule,
    ) -> Result<EvidenceCheck> {
        // `target` filter: when the requirement names a target, accept evidence
        // that names the same target OR no target at all — except for a
        // `latest` read, where only an ack of this very document proves it.
        let target_clause = match (target, version_rule) {
            (Some(_), VersionRule::Latest(_)) => "AND target = ?",
            (Some(_), _) => "AND (target = ? OR target IS NULL)",
            (None, _) => "",
        };
        let scope_clause = if scope_id.is_some() {
            "scope_id = ?"
        } else {
            "scope_id IS NULL"
        };
        let sql = format!(
            "SELECT target, doc_version, actor_id, actor_name, reason, payload FROM evidence \
             WHERE scope_kind = ? AND {scope_clause} AND kind = ? \
             AND superseded_by IS NULL {target_clause} \
             ORDER BY recorded_at DESC, id DESC LIMIT 20"
        );
        let mut q = sqlx::query(&sql).bind(scope_kind);
        if let Some(id) = scope_id {
            q = q.bind(id);
        }
        q = q.bind(kind.as_str());
        if let Some(t) = target {
            q = q.bind(t);
        }
        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        let mut reason = None;
        for row in rows {
            if let Some(fields) = required_fields.filter(|fields| !fields.is_empty()) {
                let payload_json: String = row.try_get("payload").map_err(map_row_err)?;
                let payload: serde_json::Value = serde_json::from_str(&payload_json)
                    .map_err(|e| CoreError::serde(e.to_string()))?;
                let reason_value: String = row.try_get("reason").map_err(map_row_err)?;
                let actor_id: Option<String> = row.try_get("actor_id").map_err(map_row_err)?;
                let actor_name: Option<String> = row.try_get("actor_name").map_err(map_row_err)?;
                let doc_version: Option<String> =
                    row.try_get("doc_version").map_err(map_row_err)?;
                let target_value: Option<String> = row.try_get("target").map_err(map_row_err)?;
                if let Some(field) = fields
                    .iter()
                    .find(|field| match payload.get(field.as_str()) {
                        Some(value) => value.is_null() || value.as_str() == Some(""),
                        None => match field.as_str() {
                            "reason" => reason_value.is_empty(),
                            "actor" => {
                                !actor_id.as_deref().is_some_and(|value| !value.is_empty())
                                    && !actor_name.as_deref().is_some_and(|value| !value.is_empty())
                            }
                            "doc_version" => !doc_version
                                .as_deref()
                                .is_some_and(|value| !value.is_empty()),
                            "target" => target_value.is_none(),
                            _ => true,
                        },
                    })
                {
                    reason.get_or_insert_with(|| {
                        format!("evidence payload field `{field}` is missing or empty")
                    });
                    continue;
                }
            }
            match version_rule {
                VersionRule::Any => {}
                VersionRule::AtLeast(minimum) => {
                    let version: Option<String> =
                        row.try_get("doc_version").map_err(map_row_err)?;
                    let Some(actual) = version.as_deref().and_then(parse_version) else {
                        reason.get_or_insert_with(|| {
                            format!(
                                "evidence doc_version is missing or invalid; expected an integer >= {minimum} with optional v/V prefix"
                            )
                        });
                        continue;
                    };
                    if actual < minimum {
                        reason.get_or_insert_with(|| {
                            format!(
                                "evidence doc_version {actual} is older than required {minimum}"
                            )
                        });
                        continue;
                    }
                }
                VersionRule::Latest(current) => {
                    let version: Option<String> =
                        row.try_get("doc_version").map_err(map_row_err)?;
                    if version.as_deref().and_then(parse_version) != Some(current) {
                        reason.get_or_insert_with(|| {
                            format!(
                                "evidence doc_version {} is not the current document version {current} required by min_version=latest; re-read the document and record a new document_read_ack with doc_version {current}",
                                version.as_deref().unwrap_or("<missing>")
                            )
                        });
                        continue;
                    }
                }
            }
            return Ok(EvidenceCheck {
                satisfied: true,
                reason: None,
            });
        }
        Ok(EvidenceCheck {
            satisfied: false,
            reason,
        })
    }

    /// Current `entity_versions` number of the document `doc_ref` names, or
    /// `None` when `doc_ref` is not a versioned document (e.g. a file name).
    /// Accepts both the bare UUID and the `doc_`-prefixed id.
    async fn current_document_version(&self, doc_ref: &str) -> Result<Option<u64>> {
        let entity_id = if doc_ref.starts_with("doc_") {
            doc_ref.to_string()
        } else {
            format!("doc_{doc_ref}")
        };
        let current: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(version_number) FROM entity_versions \
             WHERE entity_type = 'document' AND entity_id = ?",
        )
        .bind(entity_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(current.and_then(|version| u64::try_from(version).ok()))
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
              recorded_at, superseded_by, authenticated_actor_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
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
        .bind(ev.authenticated_actor_id.map(|i| i.to_string()))
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }
}

/// How a candidate's `doc_version` is judged (resolved once per gate check).
#[derive(Clone, Copy)]
enum VersionRule {
    Any,
    AtLeast(u64),
    Latest(u64),
}

fn parse_version(version: &str) -> Option<u64> {
    version
        .strip_prefix('v')
        .or_else(|| version.strip_prefix('V'))
        .unwrap_or(version)
        .parse()
        .ok()
}

fn select_sql(tail: &str) -> String {
    format!(
        "SELECT id, kind, scope_kind, scope_id, target, doc_version, \
         actor_kind, actor_id, actor_name, reason, payload, \
         project_id, plan_id, task_id, run_id, artifact_id, rule_id, \
         recorded_at, superseded_by, authenticated_actor_id \
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
        authenticated_actor_id: row
            .try_get::<Option<String>, _>("authenticated_actor_id")
            .map_err(map_row_err)?
            .map(|id| id.parse())
            .transpose()
            .map_err(|_| CoreError::storage("bad authenticated actor id"))?,
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
    use daruma_domain::{Actor, NewEvidence};
    use daruma_shared::{PlanId, ProjectId, TaskId};

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
            ActorRef::from_actor(&Actor::user()),
            daruma_shared::time::now(),
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
    async fn required_fields_reject_empty_or_missing_payload_values() {
        let fields = ["risk_level".to_string()];
        for payload in [
            serde_json::Value::Null,
            serde_json::json!({"summary": "checked"}),
            serde_json::json!({"risk_level": null}),
            serde_json::json!({"risk_level": ""}),
        ] {
            let repo = repo().await;
            let mut evidence = sample(RuleScope::Tenant, EvidenceKind::ImpactAssessment, None);
            evidence.payload = payload;
            apply(&repo, Event::EvidenceRecorded { evidence }).await;

            let check = repo
                .has_live_evidence(
                    &[RuleScope::Tenant],
                    EvidenceKind::ImpactAssessment,
                    None,
                    Some(&fields),
                    None,
                )
                .await
                .unwrap();
            assert!(!check.satisfied);
            assert!(check.reason.unwrap().contains("risk_level"));
        }
    }

    #[tokio::test]
    async fn required_reason_falls_back_to_top_level_column() {
        let fields = ["reason".to_string()];
        for (reason, expected) in [("готово", true), ("", false)] {
            let repo = repo().await;
            let mut evidence = sample(RuleScope::Tenant, EvidenceKind::CompletionNote, None);
            evidence.reason = reason.into();
            evidence.payload = serde_json::json!({});
            apply(&repo, Event::EvidenceRecorded { evidence }).await;

            assert_eq!(
                repo.has_live_evidence(
                    &[RuleScope::Tenant],
                    EvidenceKind::CompletionNote,
                    None,
                    Some(&fields),
                    None,
                )
                .await
                .unwrap()
                .satisfied,
                expected,
                "reason={reason:?}"
            );
        }
    }

    #[tokio::test]
    async fn required_actor_falls_back_to_identity_columns() {
        let fields = ["actor".to_string()];
        for (actor, expected) in [
            (
                ActorRef {
                    kind: "user".into(),
                    id: None,
                    name: None,
                },
                false,
            ),
            (
                ActorRef {
                    kind: "agent".into(),
                    id: None,
                    name: Some("agent".into()),
                },
                true,
            ),
            (
                ActorRef {
                    kind: "agent".into(),
                    id: Some(daruma_shared::AgentId::new()),
                    name: None,
                },
                true,
            ),
        ] {
            let repo = repo().await;
            let mut evidence = sample(RuleScope::Tenant, EvidenceKind::CompletionNote, None);
            evidence.actor = actor;
            evidence.payload = serde_json::json!({});
            apply(&repo, Event::EvidenceRecorded { evidence }).await;

            assert_eq!(
                repo.has_live_evidence(
                    &[RuleScope::Tenant],
                    EvidenceKind::CompletionNote,
                    None,
                    Some(&fields),
                    None,
                )
                .await
                .unwrap()
                .satisfied,
                expected
            );
        }
    }

    #[tokio::test]
    async fn min_version_is_numeric_and_fail_closed() {
        for (version, expected) in [
            (None, false),
            (Some("bad"), false),
            (Some("1"), false),
            (Some("3"), true),
            (Some("v3"), true),
            (Some("V3"), true),
            (Some("10"), true),
        ] {
            let repo = repo().await;
            let mut evidence = sample(
                RuleScope::Tenant,
                EvidenceKind::DocumentReadAck,
                Some("architecture.md"),
            );
            evidence.doc_version = version.map(str::to_string);
            apply(&repo, Event::EvidenceRecorded { evidence }).await;

            assert_eq!(
                repo.has_live_evidence(
                    &[RuleScope::Tenant],
                    EvidenceKind::DocumentReadAck,
                    Some("architecture.md"),
                    None,
                    Some("3"),
                )
                .await
                .unwrap()
                .satisfied,
                expected,
                "doc_version={version:?}"
            );
        }
    }

    /// A document with `appends + replaces + 1` versions (create is v1), plus
    /// evidence and document repos over the same database.
    async fn versioned_doc(
        appends: usize,
        replaces: usize,
    ) -> (
        Db,
        EvidenceRepo,
        crate::DocumentRepo,
        daruma_shared::DocumentId,
    ) {
        use daruma_domain::{DocumentKind, NewDocument};
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let evidence = EvidenceRepo::new(db.pool().clone());
        let documents = crate::DocumentRepo::new(db.pool().clone());
        let id = daruma_shared::DocumentId::new();
        let doc = NewDocument {
            id: None,
            project_id: ProjectId::new(),
            kind: DocumentKind::Interview,
            title: "Policy".into(),
            content: Some("v1 body".into()),
            status: None,
            task_id: None,
            trigger_kind: None,
            consumer: None,
        }
        .into_document(id, daruma_shared::time::now());
        let mut events = vec![Event::DocumentCreated { document: doc }];
        for n in 0..appends {
            events.push(Event::DocumentContentAppended {
                document_id: id,
                append: format!("appended {n}"),
                at: daruma_shared::time::now(),
            });
        }
        for n in 0..replaces {
            events.push(Event::DocumentContentReplaced {
                document_id: id,
                content: format!("replaced {n}"),
                at: daruma_shared::time::now(),
            });
        }
        for ev in events {
            documents
                .apply_event(&EventEnvelope::new(Actor::user(), ev))
                .await
                .unwrap();
        }
        (db, evidence, documents, id)
    }

    fn doc_ref(id: daruma_shared::DocumentId) -> String {
        id.to_string().trim_start_matches("doc_").to_string()
    }

    async fn ack(repo: &EvidenceRepo, target: Option<&str>, version: &str) -> EvidenceId {
        let mut evidence = sample(RuleScope::Tenant, EvidenceKind::DocumentReadAck, target);
        evidence.doc_version = Some(version.to_string());
        let id = evidence.id;
        apply(repo, Event::EvidenceRecorded { evidence }).await;
        id
    }

    async fn latest(repo: &EvidenceRepo, target: &str) -> EvidenceCheck {
        repo.has_live_evidence(
            &[RuleScope::Tenant],
            EvidenceKind::DocumentReadAck,
            Some(target),
            None,
            Some("latest"),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn latest_rejects_stale_ack_after_replace_and_append() {
        for (appends, replaces) in [(0, 1), (1, 0), (2, 1)] {
            let (_db, repo, _docs, id) = versioned_doc(appends, replaces).await;
            let current = 1 + appends + replaces;
            let target = doc_ref(id);
            ack(&repo, Some(&target), "1").await;

            let stale = latest(&repo, &target).await;
            assert!(!stale.satisfied, "stale v1 ack must not satisfy v{current}");
            let reason = stale.reason.unwrap();
            assert!(
                reason.contains(&format!("current document version {current}")),
                "{reason}"
            );

            ack(&repo, Some(&target), &format!("v{current}")).await;
            assert!(latest(&repo, &target).await.satisfied);
            // A rule naming the prefixed id resolves the same current version.
            let prefixed = id.to_string();
            ack(&repo, Some(&prefixed), "1").await;
            assert!(!latest(&repo, &prefixed).await.satisfied);
            ack(&repo, Some(&prefixed), &current.to_string()).await;
            assert!(latest(&repo, &prefixed).await.satisfied);
        }
    }

    #[tokio::test]
    async fn latest_accepts_current_ack_of_unchanged_document() {
        let (_db, repo, _docs, id) = versioned_doc(0, 0).await;
        let target = doc_ref(id);
        assert!(!latest(&repo, &target).await.satisfied);
        ack(&repo, Some(&target), "1").await;
        assert!(latest(&repo, &target).await.satisfied);
    }

    #[tokio::test]
    async fn latest_isolates_document_identity() {
        let (db, repo, docs, id) = versioned_doc(0, 1).await;
        let target = doc_ref(id);
        // A sibling document at the same version number, acked by its own id.
        let sibling = daruma_shared::DocumentId::new();
        let sibling_doc = daruma_domain::NewDocument {
            id: None,
            project_id: ProjectId::new(),
            kind: daruma_domain::DocumentKind::Interview,
            title: "Sibling".into(),
            content: None,
            status: None,
            task_id: None,
            trigger_kind: None,
            consumer: None,
        }
        .into_document(sibling, daruma_shared::time::now());
        docs.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentCreated {
                document: sibling_doc,
            },
        ))
        .await
        .unwrap();
        ack(&repo, Some(&doc_ref(sibling)), "2").await;
        // Untargeted ack carrying the right number proves nothing about *this* doc.
        ack(&repo, None, "2").await;
        let check = latest(&repo, &target).await;
        assert!(!check.satisfied);
        assert!(check
            .reason
            .unwrap()
            .contains(&format!("targeting `{target}`")));
        drop(db);
    }

    #[tokio::test]
    async fn latest_honours_supersede_of_current_ack() {
        let (_db, repo, _docs, id) = versioned_doc(0, 1).await;
        let target = doc_ref(id);
        let current = ack(&repo, Some(&target), "2").await;
        assert!(latest(&repo, &target).await.satisfied);
        let replacement = ack(&repo, Some(&target), "1").await;
        apply(
            &repo,
            Event::EvidenceSuperseded {
                evidence_id: current,
                superseded_by: replacement,
                at: daruma_shared::time::now(),
            },
        )
        .await;
        assert!(!latest(&repo, &target).await.satisfied);
    }

    #[tokio::test]
    async fn latest_for_unversioned_ref_keeps_legacy_match() {
        let repo = repo().await;
        ack(&repo, None, "1").await;
        assert!(latest(&repo, "architecture.md").await.satisfied);
    }

    #[tokio::test]
    async fn empty_requirements_preserve_legacy_matching() {
        let repo = repo().await;
        apply(
            &repo,
            Event::EvidenceRecorded {
                evidence: sample(RuleScope::Tenant, EvidenceKind::CompletionNote, None),
            },
        )
        .await;

        assert!(
            repo.has_live_evidence(
                &[RuleScope::Tenant],
                EvidenceKind::CompletionNote,
                None,
                Some(&[]),
                Some("latest"),
            )
            .await
            .unwrap()
            .satisfied
        );
    }

    #[tokio::test]
    async fn has_live_evidence_matches_in_chain() {
        let repo = repo().await;
        let project = ProjectId::new();
        // `document_read_ack` reaches tenant-wide: reading a document once
        // legitimately covers everything below it.
        apply(
            &repo,
            Event::EvidenceRecorded {
                evidence: sample(RuleScope::Tenant, EvidenceKind::DocumentReadAck, None),
            },
        )
        .await;
        let chain = [RuleScope::Tenant, RuleScope::Project { id: project }];
        assert!(
            repo.has_live_evidence(&chain, EvidenceKind::DocumentReadAck, None, None, None)
                .await
                .unwrap()
                .satisfied
        );
        // Wrong kind → no match. The record has to sit somewhere the query can
        // actually reach, or the assertion passes for the wrong reason: with no
        // reachable rows at all it would hold even if the kind filter were gone.
        apply(
            &repo,
            Event::EvidenceRecorded {
                evidence: sample(
                    RuleScope::Project { id: project },
                    EvidenceKind::CompletionNote,
                    None,
                ),
            },
        )
        .await;
        assert!(
            repo.has_live_evidence(&chain, EvidenceKind::CompletionNote, None, None, None)
                .await
                .unwrap()
                .satisfied
        );
        assert!(
            !repo
                .has_live_evidence(&chain, EvidenceKind::RiskCheckCompleted, None, None, None,)
                .await
                .unwrap()
                .satisfied,
            "same scope, different kind → no match"
        );
    }

    /// The reach table is what the whole cap rests on, and it is pure — so it
    /// gets checked directly rather than only through the two kinds that
    /// integration tests happen to exercise.
    #[test]
    fn reachable_caps_the_chain_per_kind() {
        let project = ProjectId::new();
        let plan = PlanId::new();
        let task = TaskId::new();
        let p = RuleScope::Project { id: project };
        let pl = RuleScope::Plan { id: plan };
        let t = RuleScope::Task { id: task };

        let full = [RuleScope::Tenant, p.clone(), pl.clone(), t.clone()];
        assert_eq!(reachable(&full, EvidenceReach::Tenant).len(), 4);
        assert_eq!(reachable(&full, EvidenceReach::Project), &full[1..]);
        assert_eq!(reachable(&full, EvidenceReach::Plan), &full[2..]);
        assert_eq!(reachable(&full, EvidenceReach::SelfOnly), &full[3..]);

        // The usual shape of a task-triggered check: no project, no plan.
        let task_chain = [RuleScope::Tenant, t.clone()];
        assert_eq!(reachable(&task_chain, EvidenceReach::Tenant).len(), 2);
        for reach in [
            EvidenceReach::Project,
            EvidenceReach::Plan,
            EvidenceReach::SelfOnly,
        ] {
            assert_eq!(
                reachable(&task_chain, reach),
                &task_chain[1..],
                "a missing level falls back inwards, never outwards: {reach:?}"
            );
        }

        // Tenant-only chain (a run or handoff check): everything collapses onto
        // the tenant, because that is genuinely the innermost thing there is.
        let tenant_only = [RuleScope::Tenant];
        for reach in [
            EvidenceReach::Tenant,
            EvidenceReach::Project,
            EvidenceReach::Plan,
            EvidenceReach::SelfOnly,
        ] {
            assert_eq!(reachable(&tenant_only, reach), &tenant_only[..]);
        }

        // The only shape where the fallback actually fires: nothing in the chain
        // is at or below the ceiling. It must land on the innermost element, not
        // on the whole chain — falling back outwards would let a tenant-wide
        // impact assessment satisfy a project's rule, which is the hole this
        // cap exists to close.
        let project_chain = [RuleScope::Tenant, p.clone()];
        assert_eq!(
            reachable(&project_chain, EvidenceReach::Plan),
            &project_chain[1..],
            "no plan in the chain → the project, never back out to the tenant"
        );

        // Plan without a project above it: project-reach must still read the
        // plan, because a plan sits inside a project even when the chain does
        // not spell it out.
        let plan_chain = [RuleScope::Tenant, pl.clone(), t.clone()];
        assert_eq!(
            reachable(&plan_chain, EvidenceReach::Project),
            &plan_chain[1..]
        );
        assert_eq!(
            reachable(&plan_chain, EvidenceReach::Plan),
            &plan_chain[1..]
        );
        assert_eq!(
            reachable(&plan_chain, EvidenceReach::SelfOnly),
            &plan_chain[2..]
        );
    }

    /// The defect this cap exists for: one tenant-scoped record used to satisfy
    /// a rule for every entity in the tenant, forever. A `completion_note` is a
    /// statement about one work unit, so it must not reach past its own scope.
    #[tokio::test]
    async fn a_tenant_wide_record_does_not_satisfy_a_self_only_kind() {
        let repo = repo().await;
        let project = ProjectId::new();
        apply(
            &repo,
            Event::EvidenceRecorded {
                evidence: sample(RuleScope::Tenant, EvidenceKind::CompletionNote, None),
            },
        )
        .await;
        let chain = [RuleScope::Tenant, RuleScope::Project { id: project }];
        assert!(
            !repo
                .has_live_evidence(&chain, EvidenceKind::CompletionNote, None, None, None)
                .await
                .unwrap()
                .satisfied,
            "tenant-wide completion_note must not satisfy a project's rule"
        );

        // Recorded where it belongs, it does satisfy.
        apply(
            &repo,
            Event::EvidenceRecorded {
                evidence: sample(
                    RuleScope::Project { id: project },
                    EvidenceKind::CompletionNote,
                    None,
                ),
            },
        )
        .await;
        assert!(
            repo.has_live_evidence(&chain, EvidenceKind::CompletionNote, None, None, None)
                .await
                .unwrap()
                .satisfied
        );
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
                at: daruma_shared::time::now(),
            },
        )
        .await;

        // Old row marked, newer still live → chain still satisfied.
        assert_eq!(
            repo.get(id).await.unwrap().unwrap().superseded_by,
            Some(newer_id)
        );
        assert!(
            repo.has_live_evidence(
                &[RuleScope::Tenant],
                EvidenceKind::ImpactAssessment,
                None,
                None,
                None,
            )
            .await
            .unwrap()
            .satisfied
        );
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
        assert!(
            repo.has_live_evidence(
                &chain,
                EvidenceKind::DocumentReadAck,
                Some("architecture.md"),
                None,
                None,
            )
            .await
            .unwrap()
            .satisfied
        );
        // Different target → not satisfied.
        assert!(
            !repo
                .has_live_evidence(
                    &chain,
                    EvidenceKind::DocumentReadAck,
                    Some("other.md"),
                    None,
                    None,
                )
                .await
                .unwrap()
                .satisfied
        );
    }
}

/// The part of a scope chain a kind of evidence may be read from.
///
/// The chain runs outermost → innermost (`[Tenant, Project?, Plan?, Task?]`),
/// so capping reach means dropping the outer prefix. Elements are optional:
/// a task-triggered check carries `[Tenant, Task]` with no project or plan.
///
/// When the chain has no element at the kind's ceiling, the fallback is the
/// innermost element — never the whole chain. That direction matters: falling
/// back outwards would reopen exactly the hole this exists to close.
/// `SelfOnly` is that fallback by definition — the scope of the thing being
/// checked, whatever it happens to be.
fn reachable(chain: &[RuleScope], reach: EvidenceReach) -> &[RuleScope] {
    fn rank(scope: &RuleScope) -> u8 {
        match scope {
            RuleScope::Tenant => 0,
            RuleScope::Project { .. } => 1,
            RuleScope::Plan { .. } => 2,
            RuleScope::Task { .. } => 3,
        }
    }
    let ceiling = match reach {
        EvidenceReach::Tenant => 0,
        EvidenceReach::Project => 1,
        EvidenceReach::Plan => 2,
        EvidenceReach::SelfOnly => return &chain[chain.len().saturating_sub(1)..],
    };
    let cut = chain
        .iter()
        .position(|s| rank(s) >= ceiling)
        .unwrap_or_else(|| chain.len().saturating_sub(1));
    &chain[cut..]
}
