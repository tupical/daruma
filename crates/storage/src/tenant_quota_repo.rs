//! Tenant quota checks for logical workspace resource limits.

use sqlx::SqlitePool;
use taskagent_domain::DEFAULT_TENANT_ID;
use taskagent_shared::{CoreError, ProjectId, Result};

#[derive(Clone)]
pub struct TenantQuotaRepo {
    pool: SqlitePool,
}

impl TenantQuotaRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn check_task_quota(&self, project_id: Option<ProjectId>) -> Result<()> {
        let tenant_id = self
            .tenant_for_project(project_id)
            .await?
            .unwrap_or_else(|| DEFAULT_TENANT_ID.to_string());
        let Some(limit) = self.limit_for(&tenant_id, "max_tasks").await? else {
            return Ok(());
        };
        let current = if tenant_id == DEFAULT_TENANT_ID {
            count_self_hosted_tasks(&self.pool).await?
        } else {
            count_tenant_tasks(&self.pool, &tenant_id).await?
        };
        reject_if_full("tasks", limit, current)
    }

    pub async fn check_plan_quota(&self, project_id: ProjectId) -> Result<()> {
        let tenant_id = self
            .tenant_for_project(Some(project_id))
            .await?
            .unwrap_or_else(|| DEFAULT_TENANT_ID.to_string());
        let Some(limit) = self.limit_for(&tenant_id, "max_plans").await? else {
            return Ok(());
        };
        let current = count_tenant_plans(&self.pool, &tenant_id).await?;
        reject_if_full("plans", limit, current)
    }

    pub async fn set_limits(
        &self,
        tenant_id: &str,
        max_tasks: Option<i64>,
        max_plans: Option<i64>,
        max_storage_mb: Option<i64>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE tenants \
             SET max_tasks = ?, max_plans = ?, max_storage_mb = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
             WHERE id = ?",
        )
        .bind(max_tasks)
        .bind(max_plans)
        .bind(max_storage_mb)
        .bind(tenant_id)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    async fn tenant_for_project(&self, project_id: Option<ProjectId>) -> Result<Option<String>> {
        let Some(project_id) = project_id else {
            return Ok(Some(DEFAULT_TENANT_ID.to_string()));
        };
        let row =
            sqlx::query_as::<_, (Option<String>,)>("SELECT tenant_id FROM projects WHERE id = ?")
                .bind(project_id.to_string())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(row.and_then(|(tenant_id,)| tenant_id))
    }

    async fn limit_for(&self, tenant_id: &str, column: &str) -> Result<Option<i64>> {
        let sql = match column {
            "max_tasks" => "SELECT max_tasks FROM tenants WHERE id = ?",
            "max_plans" => "SELECT max_plans FROM tenants WHERE id = ?",
            _ => return Err(CoreError::storage("unknown quota column")),
        };
        let row = sqlx::query_as::<_, (Option<i64>,)>(sql)
            .bind(tenant_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(row.and_then(|(limit,)| limit))
    }
}

async fn count_self_hosted_tasks(pool: &SqlitePool) -> Result<i64> {
    let row = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM tasks t \
         LEFT JOIN projects p ON p.id = t.project_id \
         WHERE t.project_id IS NULL OR p.tenant_id = ?",
    )
    .bind(DEFAULT_TENANT_ID)
    .fetch_one(pool)
    .await
    .map_err(|e| CoreError::storage(e.to_string()))?;
    Ok(row.0)
}

async fn count_tenant_tasks(pool: &SqlitePool, tenant_id: &str) -> Result<i64> {
    let row = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM tasks t \
         JOIN projects p ON p.id = t.project_id \
         WHERE p.tenant_id = ?",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .map_err(|e| CoreError::storage(e.to_string()))?;
    Ok(row.0)
}

async fn count_tenant_plans(pool: &SqlitePool, tenant_id: &str) -> Result<i64> {
    let row = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM plans pl \
         JOIN projects p ON p.id = pl.project_id \
         WHERE p.tenant_id = ?",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .map_err(|e| CoreError::storage(e.to_string()))?;
    Ok(row.0)
}

fn reject_if_full(resource: &str, limit: i64, current: i64) -> Result<()> {
    if current >= limit {
        Err(CoreError::quota_exceeded(resource, limit, current))
    } else {
        Ok(())
    }
}
