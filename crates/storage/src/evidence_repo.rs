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
    /// fields and a numeric minimum document version. Required fields are read
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
        let mut reason = None;
        for scope in reachable(chain, kind.reach()) {
            let check = self
                .scope_has(
                    scope.kind(),
                    scope.id_string(),
                    kind,
                    target,
                    required_fields,
                    min_version,
                )
                .await?;
            if check.satisfied {
                return Ok(check);
            }
            if reason.is_none() {
                reason = check.reason;
            }
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
        min_version: Option<&str>,
    ) -> Result<EvidenceCheck> {
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

        let parsed_min_version = match min_version.filter(|version| *version != "latest") {
            Some(version) => match parse_version(version) {
                Some(version) => Some(version),
                None => {
                    return Ok(EvidenceCheck {
                        satisfied: false,
                        reason: Some(format!(
                            "rule min_version `{version}` is invalid; expected an integer with optional v/V prefix"
                        )),
                    })
                }
            },
            None => None,
        };
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
            if let Some(minimum) = parsed_min_version {
                let version: Option<String> = row.try_get("doc_version").map_err(map_row_err)?;
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
                        format!("evidence doc_version {actual} is older than required {minimum}")
                    });
                    continue;
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
            ActorRef::from_actor(&Actor::User),
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
