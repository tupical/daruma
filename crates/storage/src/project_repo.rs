//! Project projection repository — materialises project-related events into the
//! `projects` SQLite table.

use crate::parse_ts;
use chrono::{DateTime, Utc};
use daruma_domain::{slugify_title, Project, DEFAULT_TENANT_ID};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{CoreError, ProjectId, Result};
use sqlx::{Row, SqlitePool};

/// Read/write access to the `projects` projection table.
pub struct ProjectRepo {
    pub(crate) pool: SqlitePool,
}

impl ProjectRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // ── queries ──────────────────────────────────────────────────────────────

    pub async fn list_all(&self) -> Result<Vec<Project>> {
        let rows = sqlx::query(
            "SELECT id, tenant_id, slug, title, description, triage_enabled, created_at, updated_at \
             FROM projects ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_project).collect()
    }

    pub async fn list_by_tenant(&self, tenant_id: &str) -> Result<Vec<Project>> {
        let rows = sqlx::query(
            "SELECT id, tenant_id, slug, title, description, triage_enabled, created_at, updated_at \
             FROM projects WHERE tenant_id = ? ORDER BY created_at ASC",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_project).collect()
    }

    pub async fn get(&self, id: ProjectId) -> Result<Option<Project>> {
        let row = sqlx::query(
            "SELECT id, tenant_id, slug, title, description, triage_enabled, created_at, updated_at \
             FROM projects WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(row_to_project).transpose()
    }

    pub async fn find_by_slug(&self, slug: &str) -> Result<Option<Project>> {
        let row = sqlx::query(
            "SELECT id, tenant_id, slug, title, description, triage_enabled, created_at, updated_at FROM ( \
                 SELECT 0 AS rank, id, tenant_id, slug, title, description, triage_enabled, created_at, updated_at \
                 FROM projects WHERE slug = ? \
                 UNION ALL \
                 SELECT 1 AS rank, p.id, p.tenant_id, p.slug, p.title, p.description, p.triage_enabled, p.created_at, p.updated_at \
                 FROM project_identifier_aliases a \
                 JOIN projects p ON p.id = a.project_id \
                 WHERE a.alias = ? \
             ) ORDER BY rank ASC LIMIT 1",
        )
        .bind(slug)
        .bind(slug)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(row_to_project).transpose()
    }

    // ── mutations ────────────────────────────────────────────────────────────

    /// Apply a single event envelope, updating the `projects` projection.
    ///
    /// Non-project events are silently ignored.
    pub async fn apply_event(&self, envelope: &EventEnvelope) -> Result<()> {
        let occurred_at = envelope.occurred_at;

        match &envelope.payload {
            Event::ProjectCreated { project } => {
                self.upsert_project(project).await?;
            }

            Event::ProjectUpdated {
                project_id,
                title,
                description,
            } => {
                // Fetch current state, apply sparse patch, then upsert.
                if let Some(mut project) = self.get(*project_id).await? {
                    if let Some(t) = title {
                        self.insert_alias(&project.slug, *project_id, occurred_at)
                            .await?;
                        self.insert_alias(&slugify_title(&project.title), *project_id, occurred_at)
                            .await?;
                        self.insert_alias(&slugify_title(t), *project_id, occurred_at)
                            .await?;
                        project.title = t.clone();
                    }
                    if let Some(d) = description {
                        project.description = d.clone();
                    }
                    project.updated_at = occurred_at;
                    self.upsert_project(&project).await?;
                }
            }

            Event::ProjectDeleted { project_id } => {
                self.delete(*project_id).await?;
            }

            // Non-project events are ignored by this repo.
            _ => {}
        }

        Ok(())
    }

    /// Delete a project row by id.
    ///
    /// Returns `true` if a row was deleted, `false` if no row matched.
    /// The caller is responsible for verifying that the project is empty
    /// (no tasks, no plans) before invoking this; the command handler does
    /// that check before emitting `Event::ProjectDeleted`.
    pub async fn delete(&self, id: ProjectId) -> Result<bool> {
        let n = sqlx::query("DELETE FROM projects WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?
            .rows_affected();
        Ok(n > 0)
    }

    pub async fn set_triage_enabled(
        &self,
        id: ProjectId,
        enabled: bool,
    ) -> Result<Option<Project>> {
        let n = sqlx::query("UPDATE projects SET triage_enabled = ?, updated_at = ? WHERE id = ?")
            .bind(enabled)
            .bind(Utc::now().to_rfc3339())
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

    // ── private helpers ───────────────────────────────────────────────────────

    /// Insert or replace a project row. Also used by the bootstrap-snapshot
    /// restore path (device-sync catch-up).
    pub async fn upsert_project(&self, project: &Project) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO projects \
             (id, tenant_id, slug, title, description, triage_enabled, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(project.id.to_string())
        .bind(project.tenant_id.as_deref().unwrap_or(DEFAULT_TENANT_ID))
        .bind(&project.slug)
        .bind(&project.title)
        .bind(&project.description)
        .bind(project.triage_enabled)
        .bind(project.created_at.to_rfc3339())
        .bind(project.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(())
    }

    async fn insert_alias(
        &self,
        alias: &str,
        project_id: ProjectId,
        created_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO project_identifier_aliases (alias, project_id, created_at) \
             VALUES (?, ?, ?)",
        )
        .bind(alias)
        .bind(project_id.to_string())
        .bind(created_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }
}

// ── row mapping ───────────────────────────────────────────────────────────────

fn row_to_project(row: &sqlx::sqlite::SqliteRow) -> Result<Project> {
    let id: String = row
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let slug: String = row
        .try_get("slug")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let tenant_id: Option<String> = row
        .try_get("tenant_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let title: String = row
        .try_get("title")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let description: Option<String> = row
        .try_get("description")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let triage_enabled: bool = row
        .try_get("triage_enabled")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let created_at_s: String = row
        .try_get("created_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let updated_at_s: String = row
        .try_get("updated_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    let project_id = id
        .parse::<ProjectId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;

    Ok(Project {
        id: project_id,
        tenant_id,
        slug,
        title,
        description,
        triage_enabled,
        created_at: parse_ts(&created_at_s)?,
        updated_at: parse_ts(&updated_at_s)?,
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_domain::Actor;
    use daruma_events::EventEnvelope;

    #[tokio::test]
    async fn project_created_and_retrieved() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = ProjectRepo::new(db.pool().clone());

        let project = Project::new("my project", Some("a description".into()));
        let id = project.id;
        let env = EventEnvelope::new(Actor::user(), Event::ProjectCreated { project });
        repo.apply_event(&env).await.unwrap();

        let found = repo.get(id).await.unwrap().expect("project should exist");
        assert_eq!(found.id, id);
        assert_eq!(found.tenant_id.as_deref(), Some(DEFAULT_TENANT_ID));
        assert_eq!(found.title, "my project");
        assert_eq!(found.description.as_deref(), Some("a description"));

        let all = repo.list_all().await.unwrap();
        assert_eq!(all.len(), 1);
    }

    #[tokio::test]
    async fn project_updated_patches_title() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = ProjectRepo::new(db.pool().clone());

        let project = Project::new("old title", None);
        let id = project.id;
        let create_env = EventEnvelope::new(Actor::user(), Event::ProjectCreated { project });
        repo.apply_event(&create_env).await.unwrap();

        let update_env = EventEnvelope::new(
            Actor::user(),
            Event::ProjectUpdated {
                project_id: id,
                title: Some("new title".into()),
                description: None,
            },
        );
        repo.apply_event(&update_env).await.unwrap();

        let updated = repo.get(id).await.unwrap().unwrap();
        assert_eq!(updated.title, "new title");

        let by_old_slug = repo
            .find_by_slug("old-title")
            .await
            .unwrap()
            .expect("old canonical slug should still resolve");
        assert_eq!(by_old_slug.id, id);

        let by_new_alias = repo
            .find_by_slug("new-title")
            .await
            .unwrap()
            .expect("new title alias should resolve");
        assert_eq!(by_new_alias.id, id);
    }

    #[tokio::test]
    async fn list_by_tenant_scopes_projects() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = ProjectRepo::new(db.pool().clone());

        for tenant_id in ["tenant-a", "tenant-b"] {
            sqlx::query(
                "INSERT INTO tenants (id, name, status, created_at, updated_at) \
                 VALUES (?, ?, 'active', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .bind(tenant_id)
            .bind(tenant_id)
            .execute(db.pool())
            .await
            .unwrap();
        }

        let mut p1 = Project::new("tenant one", None);
        p1.tenant_id = Some("tenant-a".to_string());
        let mut p2 = Project::new("tenant two", None);
        p2.tenant_id = Some("tenant-b".to_string());
        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::ProjectCreated { project: p1 },
        ))
        .await
        .unwrap();
        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::ProjectCreated { project: p2 },
        ))
        .await
        .unwrap();

        let tenant_a = repo.list_by_tenant("tenant-a").await.unwrap();

        assert_eq!(tenant_a.len(), 1);
        assert_eq!(tenant_a[0].title, "tenant one");
    }
}
