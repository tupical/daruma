//! Repo scope bindings (`repo_scopes` table, migration 0046): absolute
//! repo path → default project id. Server-side successor of the MCP
//! client's `workspaces.json` so hosted (per-tenant) MCP sessions get the
//! same scope resolution as local stdio ones. Plain config table — not
//! event-sourced.

use chrono::Utc;
use daruma_shared::{CoreError, Result};
use sqlx::{Row, SqlitePool};

pub struct RepoScopeRepo {
    pool: SqlitePool,
}

impl RepoScopeRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Every configured binding as `(scope_path, project_id)`, ordered by path.
    pub async fn list(&self) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT scope_path, project_id FROM repo_scopes ORDER BY scope_path")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get("scope_path")
                        .map_err(|e| CoreError::storage(e.to_string()))?,
                    row.try_get("project_id")
                        .map_err(|e| CoreError::storage(e.to_string()))?,
                ))
            })
            .collect()
    }

    /// Upsert a binding.
    pub async fn set(&self, scope_path: &str, project_id: &str) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO repo_scopes (scope_path, project_id, updated_at) \
             VALUES (?, ?, ?)",
        )
        .bind(scope_path)
        .bind(project_id)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Remove a binding; no-op when absent.
    pub async fn remove(&self, scope_path: &str) -> Result<()> {
        sqlx::query("DELETE FROM repo_scopes WHERE scope_path = ?")
            .bind(scope_path)
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    #[tokio::test]
    async fn set_list_remove_roundtrip() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = RepoScopeRepo::new(db.pool().clone());

        assert!(repo.list().await.unwrap().is_empty());

        repo.set("/home/u/projects/app", "prj-a").await.unwrap();
        repo.set("/home/u/projects/lib", "prj-b").await.unwrap();
        repo.set("/home/u/projects/app", "prj-c").await.unwrap(); // upsert wins

        assert_eq!(
            repo.list().await.unwrap(),
            vec![
                ("/home/u/projects/app".to_string(), "prj-c".to_string()),
                ("/home/u/projects/lib".to_string(), "prj-b".to_string()),
            ]
        );

        repo.remove("/home/u/projects/app").await.unwrap();
        assert_eq!(
            repo.list().await.unwrap(),
            vec![("/home/u/projects/lib".to_string(), "prj-b".to_string())]
        );
    }
}
