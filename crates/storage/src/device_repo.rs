use crate::parse_ts;
use daruma_shared::{time, CoreError, DeviceId, Result, Timestamp, TokenId};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    pub label: String,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<Timestamp>,
    #[serde(default)]
    pub connected: bool,
}

#[derive(Clone)]
pub struct DeviceRepo {
    pool: SqlitePool,
}

impl DeviceRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, id: DeviceId, label: &str) -> Result<Device> {
        let now = time::now();
        sqlx::query(
            "INSERT INTO devices (id, label, created_at, last_seen_at, revoked_at)
             VALUES (?, ?, ?, NULL, NULL)",
        )
        .bind(id.to_string())
        .bind(label)
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(Device {
            id,
            label: label.to_string(),
            created_at: now,
            last_seen_at: None,
            revoked_at: None,
            connected: false,
        })
    }

    pub async fn bind_token(&self, token_id: TokenId, device_id: DeviceId) -> Result<()> {
        sqlx::query("UPDATE tokens SET device_id = ? WHERE id = ?")
            .bind(device_id.to_string())
            .bind(token_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<Device>> {
        let rows = sqlx::query(
            "SELECT id, label, created_at, last_seen_at, revoked_at
             FROM devices
             ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_device).collect()
    }

    pub async fn revoke(&self, id: DeviceId) -> Result<bool> {
        let res =
            sqlx::query("UPDATE devices SET revoked_at = COALESCE(revoked_at, ?) WHERE id = ?")
                .bind(time::now().to_rfc3339())
                .bind(id.to_string())
                .execute(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn touch_last_seen_throttled(&self, id: DeviceId) -> Result<bool> {
        let now = time::now();
        let cutoff = now - chrono::Duration::minutes(1);
        let res = sqlx::query(
            "UPDATE devices
             SET last_seen_at = ?
             WHERE id = ?
               AND revoked_at IS NULL
               AND (last_seen_at IS NULL OR last_seen_at <= ?)",
        )
        .bind(now.to_rfc3339())
        .bind(id.to_string())
        .bind(cutoff.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(res.rows_affected() > 0)
    }
}

fn row_to_device(row: &sqlx::sqlite::SqliteRow) -> Result<Device> {
    let id_s: String = row
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let created_at_s: String = row
        .try_get("created_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let last_seen_at_s: Option<String> = row
        .try_get("last_seen_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let revoked_at_s: Option<String> = row
        .try_get("revoked_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    Ok(Device {
        id: id_s
            .parse::<DeviceId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        label: row
            .try_get("label")
            .map_err(|e| CoreError::storage(e.to_string()))?,
        created_at: parse_ts(&created_at_s)?,
        last_seen_at: last_seen_at_s.map(|s| parse_ts(&s)).transpose()?,
        revoked_at: revoked_at_s.map(|s| parse_ts(&s)).transpose()?,
        connected: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, TokenRepo};
    use daruma_auth::{generate, NewTokenSpec, TokenKind, TokenScope, TokenStore};
    use daruma_shared::AgentId;

    #[tokio::test]
    async fn bind_token_and_revoke_marks_token_device_revoked() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let tokens = TokenRepo::new(db.pool().clone());
        let devices = DeviceRepo::new(db.pool().clone());
        let secret = generate(NewTokenSpec {
            kind: TokenKind::Pat,
            agent_id: AgentId::new(),
            scope: TokenScope::default_user(),
            rate_limit_per_min: 60,
            expired_at: None,
        })
        .unwrap();
        tokens.insert(secret.record.clone()).await.unwrap();

        let device = devices.insert(DeviceId::new(), "laptop").await.unwrap();
        devices
            .bind_token(secret.record.id, device.id)
            .await
            .unwrap();
        devices.revoke(device.id).await.unwrap();

        let rows = tokens.list_by_prefix(&secret.record.prefix).await.unwrap();
        assert_eq!(rows[0].device_id, Some(device.id));
        assert!(rows[0].device_revoked_at.is_some());
    }

    #[tokio::test]
    async fn last_seen_is_throttled() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = DeviceRepo::new(db.pool().clone());
        let device = repo.insert(DeviceId::new(), "phone").await.unwrap();

        assert!(repo.touch_last_seen_throttled(device.id).await.unwrap());
        assert!(!repo.touch_last_seen_throttled(device.id).await.unwrap());
    }
}
