//! Per-project settings projection (`project_settings` table, migration
//! 0034). Keys: `auto_append` — the JSON [`AutoAppendSettings`], no stored
//! row means defaults (both logs ON), which also covers projects created
//! before the migration; `intake_source` — the plan-source
//! [`IntakeSourcePolicy`] (ADR-0009), no row means "not configured".

use daruma_domain::{AutoAppendSettings, IntakeSourcePolicy};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{CoreError, ProjectId, Result};
use sqlx::{Row, SqlitePool};

const AUTO_APPEND_KEY: &str = "auto_append";
const INTAKE_SOURCE_KEY: &str = "intake_source";

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

    /// Stored plan-source policy; `None` when the project has none.
    pub async fn intake_source(&self, project_id: ProjectId) -> Result<Option<IntakeSourcePolicy>> {
        let value: Option<String> = sqlx::query_scalar(
            "SELECT value FROM project_settings WHERE project_id = ? AND key = ?",
        )
        .bind(project_id.to_string())
        .bind(INTAKE_SOURCE_KEY)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        value
            .map(|v| serde_json::from_str(&v).map_err(|e| CoreError::serde(e.to_string())))
            .transpose()
    }

    /// Apply settings events to the projection.
    pub async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        if let Event::ProjectSettingsChanged {
            project_id,
            auto_append,
            at,
            intake_source,
        } = &env.payload
        {
            match intake_source {
                None => {}
                Some(None) => {
                    sqlx::query("DELETE FROM project_settings WHERE project_id = ? AND key = ?")
                        .bind(project_id.to_string())
                        .bind(INTAKE_SOURCE_KEY)
                        .execute(&self.pool)
                        .await
                        .map_err(|e| CoreError::storage(e.to_string()))?;
                }
                Some(Some(policy)) => {
                    let value = serde_json::to_string(policy)
                        .map_err(|e| CoreError::serde(e.to_string()))?;
                    sqlx::query(
                        "INSERT OR REPLACE INTO project_settings (project_id, key, value, updated_at) \
                         VALUES (?, ?, ?, ?)",
                    )
                    .bind(project_id.to_string())
                    .bind(INTAKE_SOURCE_KEY)
                    .bind(value)
                    .bind(at.to_rfc3339())
                    .execute(&self.pool)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                }
            }
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
                intake_source: None,
            },
        );
        repo.apply_event(&env).await.unwrap();
        let s = repo.auto_append(project).await.unwrap();
        assert!(!s.interview);
        assert!(s.human_log);
    }

    #[tokio::test]
    async fn intake_source_set_kept_by_legacy_event_then_removed() {
        use daruma_domain::IntakeSourceMode;
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = ProjectSettingsRepo::new(db.pool().clone());
        let project = ProjectId::new();
        assert!(repo.intake_source(project).await.unwrap().is_none());

        let policy = IntakeSourcePolicy {
            mode: IntakeSourceMode::Enforce,
            ..IntakeSourcePolicy::default()
        };
        let apply = |intake_source| {
            let repo = &repo;
            async move {
                let env = EventEnvelope::new(
                    Actor::user(),
                    Event::ProjectSettingsChanged {
                        project_id: project,
                        auto_append: AutoAppendSettings::default(),
                        at: chrono::Utc::now(),
                        intake_source,
                    },
                );
                repo.apply_event(&env).await.unwrap();
            }
        };
        apply(Some(Some(policy.clone()))).await;
        assert_eq!(
            repo.intake_source(project).await.unwrap(),
            Some(policy.clone())
        );

        // A pre-ADR-0009 event has no `intake_source` key at all: it must
        // deserialize and leave the stored policy untouched.
        let legacy = serde_json::json!({
            "type": "project_settings_changed",
            "project_id": project,
            "auto_append": {"interview": false, "human_log": true},
            "at": chrono::Utc::now(),
        });
        let payload: Event = serde_json::from_value(legacy).unwrap();
        assert!(matches!(
            &payload,
            Event::ProjectSettingsChanged {
                intake_source: None,
                ..
            }
        ));
        repo.apply_event(&EventEnvelope::new(Actor::user(), payload))
            .await
            .unwrap();
        assert!(!repo.auto_append(project).await.unwrap().interview);
        assert_eq!(repo.intake_source(project).await.unwrap(), Some(policy));

        apply(Some(None)).await;
        assert!(repo.intake_source(project).await.unwrap().is_none());
    }
}
