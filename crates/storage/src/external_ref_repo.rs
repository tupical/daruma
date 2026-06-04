//! ExternalRef repository — cross-system identity mapping with composite-PK
//! idempotency guard.

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use taskagent_domain::ExternalRef;
use taskagent_shared::{CoreError, Result};

/// Read/write access to the `external_refs` table.
pub struct ExternalRefRepo {
    pub(crate) pool: SqlitePool,
}

impl ExternalRefRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── queries ──────────────────────────────────────────────────────────────

    /// Look up the `internal_id` for a given `(tenant, kind, external_id)` triple.
    pub async fn lookup(
        &self,
        tenant: &str,
        kind: &str,
        external_id: &str,
    ) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT internal_id FROM external_refs \
             WHERE tenant = ? AND kind = ? AND external_id = ?",
        )
        .bind(tenant)
        .bind(kind)
        .bind(external_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        match row {
            None => Ok(None),
            Some(r) => {
                let internal_id: String = r
                    .try_get("internal_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(Some(internal_id))
            }
        }
    }

    // ── mutations ────────────────────────────────────────────────────────────

    /// Insert a new external ref.  The composite PK `(tenant, kind, external_id)`
    /// is unique, so a duplicate insertion will return a storage error — the
    /// caller should [`lookup`] first when implementing idempotent creation.
    pub async fn insert(&self, ext: &ExternalRef) -> Result<()> {
        sqlx::query(
            "INSERT INTO external_refs (tenant, kind, external_id, internal_id, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&ext.tenant)
        .bind(&ext.kind)
        .bind(&ext.external_id)
        .bind(&ext.internal_id)
        .bind(ext.created_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(())
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

#[allow(dead_code)]
fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| CoreError::serde(e.to_string()))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use taskagent_shared::{time, PlanId};

    async fn make_repo() -> (Db, ExternalRefRepo) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = ExternalRefRepo::new(db.pool().clone());
        (db, repo)
    }

    fn make_ext(plan_id: PlanId) -> ExternalRef {
        ExternalRef {
            tenant: "omc".to_string(),
            kind: "plan".to_string(),
            external_id: "plan-ext-001".to_string(),
            internal_id: plan_id.to_string(),
            created_at: time::now(),
        }
    }

    #[tokio::test]
    async fn external_ref_insert_and_lookup() {
        let (_db, repo) = make_repo().await;
        let plan_id = PlanId::new();
        let ext = make_ext(plan_id);

        repo.insert(&ext).await.unwrap();

        let found = repo.lookup("omc", "plan", "plan-ext-001").await.unwrap();
        assert_eq!(found, Some(plan_id.to_string()));
    }

    #[tokio::test]
    async fn external_ref_lookup_miss_returns_none() {
        let (_db, repo) = make_repo().await;
        let result = repo.lookup("omc", "plan", "nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn external_ref_duplicate_insert_is_rejected() {
        let (_db, repo) = make_repo().await;
        let plan_id = PlanId::new();
        let ext = make_ext(plan_id);

        repo.insert(&ext).await.unwrap();

        // Second insert with the same PK must fail.
        let result = repo.insert(&ext).await;
        assert!(result.is_err(), "duplicate insert should be rejected by PK");
    }

    #[tokio::test]
    async fn external_ref_different_tenants_are_independent() {
        let (_db, repo) = make_repo().await;
        let p1 = PlanId::new();
        let p2 = PlanId::new();

        repo.insert(&ExternalRef {
            tenant: "omc".to_string(),
            kind: "plan".to_string(),
            external_id: "shared-ext-id".to_string(),
            internal_id: p1.to_string(),
            created_at: time::now(),
        })
        .await
        .unwrap();

        repo.insert(&ExternalRef {
            tenant: "github".to_string(),
            kind: "plan".to_string(),
            external_id: "shared-ext-id".to_string(),
            internal_id: p2.to_string(),
            created_at: time::now(),
        })
        .await
        .unwrap();

        let omc = repo.lookup("omc", "plan", "shared-ext-id").await.unwrap();
        let gh = repo
            .lookup("github", "plan", "shared-ext-id")
            .await
            .unwrap();
        assert_eq!(omc, Some(p1.to_string()));
        assert_eq!(gh, Some(p2.to_string()));
    }
}
