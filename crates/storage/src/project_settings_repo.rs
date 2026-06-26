//! Per-project settings projection (`project_settings` table, migration
//! 0034). Currently a single key — `auto_append` — holding the JSON
//! [`AutoAppendSettings`]. No stored row means defaults (both logs ON),
//! which also covers projects created before the migration.

use sqlx::{Row, SqlitePool};
use daruma_domain::AutoAppendSettings;
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{CoreError, ProjectId, Result};

const AUTO_APPEND_KEY: &str = "auto_append";

pub struct ProjectSettingsRepo {
    pool: SqlitePool,
}

impl ProjectSettingsRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Current auto-append settings for a project; defaults when unset.
    pub async fn auto_append(&self, project_id: ProjectId) -> Result<AutoAppendSettings> {
        let row =
            sqlx::query("SELECT value FROM project_settings WHERE project_id = ? AND key = ?")
                .bind(project_id.to_string())
                .bind(AUTO_APPEND_KEY)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;

        match row {
            None => Ok(AutoAppendSettings::default()),
            Some(row) => {
                let value: String = row
                    .try_get("value")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                serde_json::from_str(&value).map_err(|e| CoreError::serde(e.to_string()))
            }
        }
    }

    /// Apply settings events to the projection.
    pub async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        if let Event::ProjectSettingsChanged {
            project_id,
            auto_append,
            at,
        } = &env.payload
        {
            let value =
                serde_json::to_string(auto_append).map_err(|e| CoreError::serde(e.to_string()))?;
            sqlx::query(
                "INSERT OR REPLACE INTO project_settings (project_id, key, value, updated_at) \
                 VALUES (?, ?, ?, ?)",
            )
            .bind(project_id.to_string())
            .bind(AUTO_APPEND_KEY)
            .bind(value)
            .bind(at.to_rfc3339())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_domain::Actor;

    #[tokio::test]
    async fn defaults_then_event_roundtrip() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = ProjectSettingsRepo::new(db.pool().clone());
        let project = ProjectId::new();

        let s = repo.auto_append(project).await.unwrap();
        assert!(s.interview && s.human_log, "missing row = defaults ON");

        let env = EventEnvelope::new(
            Actor::user(),
            Event::ProjectSettingsChanged {
                project_id: project,
                auto_append: AutoAppendSettings {
                    interview: false,
                    human_log: true,
                },
                at: chrono::Utc::now(),
            },
        );
        repo.apply_event(&env).await.unwrap();
        let s = repo.auto_append(project).await.unwrap();
        assert!(!s.interview);
        assert!(s.human_log);
    }
}
