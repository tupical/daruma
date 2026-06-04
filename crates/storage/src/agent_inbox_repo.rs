//! Per-agent inbox cursor — backs the `/v1/agents/{id}/inbox` long-poll
//! endpoint and the `inbox/ack` cursor update.

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use taskagent_shared::{time, AgentId, CoreError, Result};

/// Read/write access to the `agent_acks` table.
#[derive(Clone)]
pub struct AgentInboxRepo {
    pool: SqlitePool,
}

impl AgentInboxRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Highest `seq` the agent has ack'd. Returns `0` if the agent has no
    /// row yet (i.e. it has never ack'd).
    pub async fn get_cursor(&self, agent_id: AgentId) -> Result<u64> {
        let row = sqlx::query("SELECT last_acked_seq FROM agent_acks WHERE agent_id = ?")
            .bind(agent_id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        match row {
            Some(r) => {
                let seq: i64 = r
                    .try_get("last_acked_seq")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(seq.max(0) as u64)
            }
            None => Ok(0),
        }
    }

    /// Set the cursor to `max(current, up_to_seq)`. Idempotent and
    /// monotonic — re-acking an older seq does nothing.
    pub async fn ack(&self, agent_id: AgentId, up_to_seq: u64) -> Result<u64> {
        let now = time::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO agent_acks (agent_id, last_acked_seq, updated_at) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT(agent_id) DO UPDATE SET \
                 last_acked_seq = MAX(last_acked_seq, excluded.last_acked_seq), \
                 updated_at = excluded.updated_at",
        )
        .bind(agent_id.to_string())
        .bind(up_to_seq as i64)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        self.get_cursor(agent_id).await
    }

    /// Timestamp of the last ack, if any.
    pub async fn last_ack_at(&self, agent_id: AgentId) -> Result<Option<DateTime<Utc>>> {
        let row = sqlx::query("SELECT updated_at FROM agent_acks WHERE agent_id = ?")
            .bind(agent_id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        let Some(r) = row else { return Ok(None) };
        let s: String = r
            .try_get("updated_at")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(Some(
            DateTime::parse_from_rfc3339(&s)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| CoreError::serde(e.to_string()))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    #[tokio::test]
    async fn cursor_starts_at_zero_for_unknown_agent() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = AgentInboxRepo::new(db.pool().clone());

        let id = AgentId::new();
        assert_eq!(repo.get_cursor(id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn ack_is_monotonic() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = AgentInboxRepo::new(db.pool().clone());

        let id = AgentId::new();
        assert_eq!(repo.ack(id, 5).await.unwrap(), 5);
        assert_eq!(repo.ack(id, 10).await.unwrap(), 10);
        assert_eq!(
            repo.ack(id, 3).await.unwrap(),
            10,
            "older ack must not regress"
        );
        assert_eq!(repo.get_cursor(id).await.unwrap(), 10);
    }
}
