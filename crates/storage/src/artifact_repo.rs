//! Artifact Registry repository — projection over `artifacts` and
//! `artifact_relations` (migration 0036_artifact_registry).
//!
//! Validates fencing tokens on `ArtifactWriteCommitted` so a stale holder
//! cannot commit writes after losing its lease.

use crate::parse_ts;
use daruma_domain::{Artifact, ArtifactRelation, ArtifactRelationKind, ArtifactStatus};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{ArtifactId, CoreError, ProjectId, Result, TaskId};
use sqlx::{Row, SqlitePool};

pub struct ArtifactRepo {
    pool: SqlitePool,
}

impl ArtifactRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── queries ────────────────────────────────────────────────────────────────

    pub async fn get(&self, id: ArtifactId) -> Result<Option<Artifact>> {
        let row = sqlx::query(&select_sql("WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        row.as_ref().map(row_to_artifact).transpose()
    }

    pub async fn get_by_uri(&self, uri: &str) -> Result<Option<Artifact>> {
        let row = sqlx::query(&select_sql("WHERE uri = ?"))
            .bind(uri)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        row.as_ref().map(row_to_artifact).transpose()
    }

    /// List artifacts, optionally scoped to a project, task, and/or status.
    ///
    /// Filters compose with `AND`; binds are appended in the same order the
    /// conditions are pushed so the placeholders line up.
    pub async fn list(
        &self,
        project_id: Option<ProjectId>,
        task_id: Option<TaskId>,
        status: Option<ArtifactStatus>,
    ) -> Result<Vec<Artifact>> {
        let mut conditions: Vec<&str> = Vec::new();
        if project_id.is_some() {
            conditions.push("project_id = ?");
        }
        if task_id.is_some() {
            conditions.push("task_id = ?");
        }
        if status.is_some() {
            conditions.push("status = ?");
        }
        let filter = if conditions.is_empty() {
            "ORDER BY created_at".to_string()
        } else {
            format!("WHERE {} ORDER BY created_at", conditions.join(" AND "))
        };

        let sql = select_sql(&filter);
        let mut q = sqlx::query(&sql);
        if let Some(p) = project_id {
            q = q.bind(p.to_string());
        }
        if let Some(t) = task_id {
            q = q.bind(t.to_string());
        }
        if let Some(s) = status {
            q = q.bind(s.as_str());
        }

        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_artifact).collect()
    }

    pub async fn relations_for(&self, id: ArtifactId) -> Result<Vec<ArtifactRelation>> {
        let rows = sqlx::query(
            "SELECT id, from_id, to_id, kind, created_at FROM artifact_relations
             WHERE from_id = ? OR to_id = ? ORDER BY created_at",
        )
        .bind(id.to_string())
        .bind(id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.iter().map(row_to_relation).collect()
    }

    // ── event projection ───────────────────────────────────────────────────────

    pub async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        let at = env.occurred_at;
        match &env.payload {
            Event::ArtifactRegistered { artifact } => self.upsert(artifact).await,

            Event::ArtifactOwnerAssigned {
                artifact_id,
                owner_agent_id,
                ..
            } => {
                sqlx::query("UPDATE artifacts SET owner_agent_id = ?, updated_at = ? WHERE id = ?")
                    .bind(owner_agent_id.to_string())
                    .bind(at.to_rfc3339())
                    .bind(artifact_id.to_string())
                    .execute(&self.pool)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(())
            }

            Event::ArtifactStatusChanged {
                artifact_id, to, ..
            } => {
                sqlx::query("UPDATE artifacts SET status = ?, updated_at = ? WHERE id = ?")
                    .bind(to.as_str())
                    .bind(at.to_rfc3339())
                    .bind(artifact_id.to_string())
                    .execute(&self.pool)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(())
            }

            Event::ArtifactChanged {
                artifact_id,
                title,
                description,
                ..
            } => {
                if let Some(t) = title {
                    sqlx::query("UPDATE artifacts SET title = ?, updated_at = ? WHERE id = ?")
                        .bind(t)
                        .bind(at.to_rfc3339())
                        .bind(artifact_id.to_string())
                        .execute(&self.pool)
                        .await
                        .map_err(|e| CoreError::storage(e.to_string()))?;
                }
                if let Some(d) = description {
                    sqlx::query(
                        "UPDATE artifacts SET description = ?, updated_at = ? WHERE id = ?",
                    )
                    .bind(d)
                    .bind(at.to_rfc3339())
                    .bind(artifact_id.to_string())
                    .execute(&self.pool)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                }
                Ok(())
            }

            Event::ArtifactWriteCommitted {
                artifact_id,
                agent_id,
                fencing_token,
                version,
                ..
            } => {
                // Validate fencing token before committing: the artifact's uri
                // must have a live lease held by agent_id carrying this token.
                // We look up the artifact uri first, then call check_fencing_token
                // on the lease table.  A stale token → record the event but do NOT
                // update last_write_token / version (the write is rejected).
                let artifact = self.get(*artifact_id).await?;
                let token_valid = if let Some(a) = &artifact {
                    sqlx::query(
                        "SELECT 1 FROM work_leases \
                         WHERE agent_id = ? AND target_uri = ? \
                           AND fencing_token = ? AND expires_at >= ?",
                    )
                    .bind(agent_id.to_string())
                    .bind(&a.uri)
                    .bind(fencing_token)
                    .bind(chrono::Utc::now().to_rfc3339())
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?
                    .is_some()
                } else {
                    false
                };

                if token_valid {
                    sqlx::query(
                        "UPDATE artifacts \
                         SET last_write_token = ?, version = ?, status = 'committed', \
                             updated_at = ? \
                         WHERE id = ?",
                    )
                    .bind(fencing_token)
                    .bind(version.as_deref())
                    .bind(at.to_rfc3339())
                    .bind(artifact_id.to_string())
                    .execute(&self.pool)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                }
                // Stale token: event is recorded in the event log for audit but
                // the projection row is intentionally NOT updated.
                Ok(())
            }

            Event::ArtifactDeprecated { artifact_id, .. } => {
                sqlx::query(
                    "UPDATE artifacts SET status = 'deprecated', updated_at = ? WHERE id = ?",
                )
                .bind(at.to_rfc3339())
                .bind(artifact_id.to_string())
                .execute(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(())
            }

            Event::ArtifactRelationAdded { relation } => {
                sqlx::query(
                    "INSERT OR IGNORE INTO artifact_relations (id, from_id, to_id, kind, created_at)
                     VALUES (?, ?, ?, ?, ?)",
                )
                .bind(relation.id.to_string())
                .bind(relation.from_id.to_string())
                .bind(relation.to_id.to_string())
                .bind(relation.kind.as_str())
                .bind(relation.created_at.to_rfc3339())
                .execute(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(())
            }

            Event::ArtifactRelationRemoved {
                from_id,
                to_id,
                kind,
                ..
            } => {
                sqlx::query(
                    "DELETE FROM artifact_relations WHERE from_id = ? AND to_id = ? AND kind = ?",
                )
                .bind(from_id.to_string())
                .bind(to_id.to_string())
                .bind(kind.as_str())
                .execute(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(())
            }

            _ => Ok(()),
        }
    }

    async fn upsert(&self, a: &Artifact) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO artifacts \
             (id, uri, title, description, status, owner_agent_id, task_id, project_id, \
              version, last_write_token, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(a.id.to_string())
        .bind(&a.uri)
        .bind(&a.title)
        .bind(&a.description)
        .bind(a.status.as_str())
        .bind(a.owner_agent_id.map(|x| x.to_string()))
        .bind(a.task_id.map(|x| x.to_string()))
        .bind(a.project_id.map(|x| x.to_string()))
        .bind(a.version.as_deref())
        .bind(a.last_write_token)
        .bind(a.created_at.to_rfc3339())
        .bind(a.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }
}

// ── row helpers ────────────────────────────────────────────────────────────────

fn select_sql(filter: &str) -> String {
    format!(
        "SELECT id, uri, title, description, status, owner_agent_id, task_id, project_id, \
         version, last_write_token, created_at, updated_at FROM artifacts {filter}"
    )
}

fn row_to_artifact(r: &sqlx::sqlite::SqliteRow) -> Result<Artifact> {
    fn col<T: for<'r> sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>>(
        r: &sqlx::sqlite::SqliteRow,
        name: &'static str,
    ) -> Result<T> {
        r.try_get(name)
            .map_err(|e| CoreError::storage(e.to_string()))
    }

    let id: String = col(r, "id")?;
    let status_s: String = col(r, "status")?;
    let owner: Option<String> = col(r, "owner_agent_id")?;
    let task: Option<String> = col(r, "task_id")?;
    let project: Option<String> = col(r, "project_id")?;
    let created: String = col(r, "created_at")?;
    let updated: String = col(r, "updated_at")?;

    Ok(Artifact {
        id: id
            .parse()
            .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?,
        uri: col(r, "uri")?,
        title: col(r, "title")?,
        description: col(r, "description")?,
        status: ArtifactStatus::parse(&status_s)
            .ok_or_else(|| CoreError::serde(format!("unknown artifact status: {status_s}")))?,
        owner_agent_id: owner
            .map(|s| s.parse())
            .transpose()
            .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?,
        task_id: task
            .map(|s| s.parse())
            .transpose()
            .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?,
        project_id: project
            .map(|s| s.parse())
            .transpose()
            .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?,
        version: col(r, "version")?,
        last_write_token: col(r, "last_write_token")?,
        created_at: parse_ts(&created)?,
        updated_at: parse_ts(&updated)?,
    })
}

fn row_to_relation(r: &sqlx::sqlite::SqliteRow) -> Result<ArtifactRelation> {
    let id: String = r
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let from: String = r
        .try_get("from_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let to: String = r
        .try_get("to_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let kind_s: String = r
        .try_get("kind")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let created: String = r
        .try_get("created_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    Ok(ArtifactRelation {
        id: id
            .parse()
            .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?,
        from_id: from
            .parse()
            .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?,
        to_id: to
            .parse()
            .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?,
        kind: ArtifactRelationKind::parse(&kind_s)
            .ok_or_else(|| CoreError::serde(format!("unknown artifact relation kind: {kind_s}")))?,
        created_at: parse_ts(&created)?,
    })
}

// ── tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_domain::{Actor, Artifact, ArtifactRelation, ArtifactRelationKind, ArtifactStatus};
    use daruma_events::{Event, EventEnvelope};
    use daruma_shared::{AgentId, ArtifactId, ArtifactRelationId};

    async fn repo() -> (Db, ArtifactRepo) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let r = ArtifactRepo::new(db.pool().clone());
        (db, r)
    }

    fn sample_artifact(uri: &str) -> Artifact {
        let now = chrono::Utc::now();
        Artifact {
            id: ArtifactId::new(),
            uri: uri.to_string(),
            title: format!("Artifact {uri}"),
            description: String::new(),
            status: ArtifactStatus::Pending,
            owner_agent_id: None,
            task_id: None,
            project_id: None,
            version: None,
            last_write_token: None,
            created_at: now,
            updated_at: now,
        }
    }

    async fn seed(r: &ArtifactRepo, uri: &str) -> Artifact {
        let a = sample_artifact(uri);
        let env = EventEnvelope::new(
            Actor::user(),
            Event::ArtifactRegistered {
                artifact: a.clone(),
            },
        );
        r.apply_event(&env).await.unwrap();
        a
    }

    #[tokio::test]
    async fn register_and_get_by_id_and_uri() {
        let (_db, r) = repo().await;
        let a = seed(&r, "artifact://api/users").await;

        let by_id = r.get(a.id).await.unwrap().expect("found by id");
        assert_eq!(by_id.uri, "artifact://api/users");

        let by_uri = r
            .get_by_uri("artifact://api/users")
            .await
            .unwrap()
            .expect("found by uri");
        assert_eq!(by_uri.id, a.id);
    }

    #[tokio::test]
    async fn owner_assigned_updates_row() {
        let (_db, r) = repo().await;
        let a = seed(&r, "artifact://api/schema").await;
        let agent = AgentId::new();
        let now = chrono::Utc::now();

        let env = EventEnvelope::new(
            Actor::user(),
            Event::ArtifactOwnerAssigned {
                artifact_id: a.id,
                owner_agent_id: agent,
                at: now,
            },
        );
        r.apply_event(&env).await.unwrap();

        let updated = r.get(a.id).await.unwrap().unwrap();
        assert_eq!(updated.owner_agent_id, Some(agent));
    }

    #[tokio::test]
    async fn status_changed_updates_row() {
        let (_db, r) = repo().await;
        let a = seed(&r, "artifact://svc/queue").await;
        let now = chrono::Utc::now();

        let env = EventEnvelope::new(
            Actor::user(),
            Event::ArtifactStatusChanged {
                artifact_id: a.id,
                from: ArtifactStatus::Pending,
                to: ArtifactStatus::Active,
                at: now,
            },
        );
        r.apply_event(&env).await.unwrap();

        assert_eq!(
            r.get(a.id).await.unwrap().unwrap().status,
            ArtifactStatus::Active
        );
    }

    #[tokio::test]
    async fn stale_fencing_token_write_rejected() {
        let (_db, r) = repo().await;
        let a = seed(&r, "artifact://db/migrations").await;
        let agent = AgentId::new();
        let now = chrono::Utc::now();

        // No active lease exists — stale token.
        let env = EventEnvelope::new(
            Actor::user(),
            Event::ArtifactWriteCommitted {
                artifact_id: a.id,
                agent_id: agent,
                fencing_token: 42,
                version: Some("v1".into()),
                at: now,
            },
        );
        r.apply_event(&env).await.unwrap();

        // Projection must NOT be updated because no live lease with token=42 exists.
        let row = r.get(a.id).await.unwrap().unwrap();
        assert_eq!(row.status, ArtifactStatus::Pending);
        assert!(row.version.is_none());
        assert!(row.last_write_token.is_none());
    }

    #[tokio::test]
    async fn list_filters_by_status() {
        let (_db, r) = repo().await;
        let pending = seed(&r, "artifact://svc/pending").await;
        let active = seed(&r, "artifact://svc/active").await;

        // Flip one artifact to `active` via a status-change event.
        let env = EventEnvelope::new(
            Actor::user(),
            Event::ArtifactStatusChanged {
                artifact_id: active.id,
                from: ArtifactStatus::Pending,
                to: ArtifactStatus::Active,
                at: chrono::Utc::now(),
            },
        );
        r.apply_event(&env).await.unwrap();

        let all = r.list(None, None, None).await.unwrap();
        assert_eq!(all.len(), 2);

        let only_active = r
            .list(None, None, Some(ArtifactStatus::Active))
            .await
            .unwrap();
        assert_eq!(only_active.len(), 1);
        assert_eq!(only_active[0].id, active.id);

        let only_pending = r
            .list(None, None, Some(ArtifactStatus::Pending))
            .await
            .unwrap();
        assert_eq!(only_pending.len(), 1);
        assert_eq!(only_pending[0].id, pending.id);
    }

    #[tokio::test]
    async fn deprecated_event_sets_status() {
        let (_db, r) = repo().await;
        let a = seed(&r, "artifact://api/v1/legacy").await;
        let now = chrono::Utc::now();

        let env = EventEnvelope::new(
            Actor::user(),
            Event::ArtifactDeprecated {
                artifact_id: a.id,
                reason: Some("superseded by v2".into()),
                at: now,
            },
        );
        r.apply_event(&env).await.unwrap();

        assert_eq!(
            r.get(a.id).await.unwrap().unwrap().status,
            ArtifactStatus::Deprecated
        );
    }

    #[tokio::test]
    async fn relation_added_and_removed() {
        let (_db, r) = repo().await;
        let a1 = seed(&r, "artifact://svc/auth").await;
        let a2 = seed(&r, "contract://auth@v1").await;
        let now = chrono::Utc::now();
        let rel_id = ArtifactRelationId::new();

        let add_env = EventEnvelope::new(
            Actor::user(),
            Event::ArtifactRelationAdded {
                relation: ArtifactRelation {
                    id: rel_id,
                    from_id: a1.id,
                    to_id: a2.id,
                    kind: ArtifactRelationKind::Implements,
                    created_at: now,
                },
            },
        );
        r.apply_event(&add_env).await.unwrap();

        let rels = r.relations_for(a1.id).await.unwrap();
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].kind, ArtifactRelationKind::Implements);

        let rm_env = EventEnvelope::new(
            Actor::user(),
            Event::ArtifactRelationRemoved {
                relation_id: rel_id,
                from_id: a1.id,
                to_id: a2.id,
                kind: ArtifactRelationKind::Implements,
                at: now,
            },
        );
        r.apply_event(&rm_env).await.unwrap();

        assert!(r.relations_for(a1.id).await.unwrap().is_empty());
    }
}
