//! Local outbox for commands/events created while the desktop is offline.

#![allow(dead_code)] // Wired into local_executor/reconnect flush in the next Phase 2 block.

use taskagent_core::embed::{Db, EventEnvelope};
use taskagent_shared::{CoreError, DeviceId, Result};

#[derive(Clone, Debug)]
pub struct OutboxEntry {
    pub id: i64,
    pub origin_device_id: DeviceId,
    pub origin_seq: u64,
    pub envelope: EventEnvelope,
}

pub struct Outbox {
    db: Db,
}

impl Outbox {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub async fn ensure_schema(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS desktop_outbox (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                origin_device_id TEXT NOT NULL,
                origin_seq INTEGER NOT NULL,
                envelope_json TEXT NOT NULL,
                flushed_at TEXT NULL,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                UNIQUE(origin_device_id, origin_seq)
            )",
        )
        .execute(self.db.pool())
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_desktop_outbox_pending
             ON desktop_outbox (flushed_at, origin_device_id, origin_seq)",
        )
        .execute(self.db.pool())
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    pub async fn enqueue(
        &self,
        origin_device_id: DeviceId,
        origin_seq: u64,
        mut envelope: EventEnvelope,
    ) -> Result<bool> {
        envelope.origin_device_id = Some(origin_device_id);
        envelope.origin_seq = origin_seq;
        let envelope_json =
            serde_json::to_string(&envelope).map_err(|e| CoreError::serde(e.to_string()))?;
        let res = sqlx::query(
            "INSERT OR IGNORE INTO desktop_outbox
             (origin_device_id, origin_seq, envelope_json)
             VALUES (?, ?, ?)",
        )
        .bind(origin_device_id.to_string())
        .bind(origin_seq as i64)
        .bind(envelope_json)
        .execute(self.db.pool())
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn pending(&self, limit: u32) -> Result<Vec<OutboxEntry>> {
        let rows = sqlx::query_as::<_, OutboxRow>(
            "SELECT id, origin_device_id, origin_seq, envelope_json
             FROM desktop_outbox
             WHERE flushed_at IS NULL
             ORDER BY origin_device_id, origin_seq
             LIMIT ?",
        )
        .bind(limit.max(1) as i64)
        .fetch_all(self.db.pool())
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.into_iter().map(OutboxEntry::try_from).collect()
    }

    pub async fn next_origin_seq(&self, origin_device_id: DeviceId) -> Result<u64> {
        let row = sqlx::query_as::<_, (Option<i64>,)>(
            "SELECT MAX(origin_seq) FROM desktop_outbox WHERE origin_device_id = ?",
        )
        .bind(origin_device_id.to_string())
        .fetch_one(self.db.pool())
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(row.0.unwrap_or(0).max(0) as u64 + 1)
    }

    pub async fn mark_flushed(&self, id: i64) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE desktop_outbox
             SET flushed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ? AND flushed_at IS NULL",
        )
        .bind(id)
        .execute(self.db.pool())
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(res.rows_affected() > 0)
    }
}

#[derive(sqlx::FromRow)]
struct OutboxRow {
    id: i64,
    origin_device_id: String,
    origin_seq: i64,
    envelope_json: String,
}

impl TryFrom<OutboxRow> for OutboxEntry {
    type Error = CoreError;

    fn try_from(row: OutboxRow) -> Result<Self> {
        let origin_device_id = row
            .origin_device_id
            .parse::<DeviceId>()
            .map_err(|e| CoreError::serde(e.to_string()))?;
        let envelope = serde_json::from_str(&row.envelope_json)
            .map_err(|e| CoreError::serde(e.to_string()))?;
        Ok(Self {
            id: row.id,
            origin_device_id,
            origin_seq: row.origin_seq.max(0) as u64,
            envelope,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use taskagent_core::embed::{Event, EventEnvelope};
    use taskagent_domain::{Actor, NewTask};

    #[tokio::test]
    async fn enqueue_orders_and_marks_pending_events() {
        let db = Db::memory().await.unwrap();
        let outbox = Outbox::new(db);
        outbox.ensure_schema().await.unwrap();
        let device = DeviceId::new();

        let envelope = EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated {
                task: NewTask::new("offline"),
            },
        );

        assert!(outbox.enqueue(device, 2, envelope.clone()).await.unwrap());
        assert!(outbox.enqueue(device, 1, envelope).await.unwrap());
        assert!(!outbox
            .enqueue(
                device,
                1,
                EventEnvelope::new(
                    Actor::user(),
                    Event::TaskCreated {
                        task: NewTask::new("duplicate"),
                    },
                ),
            )
            .await
            .unwrap());

        let pending = outbox.pending(10).await.unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].origin_seq, 1);
        assert_eq!(pending[1].origin_seq, 2);
        assert_eq!(pending[0].envelope.origin_device_id, Some(device));

        assert!(outbox.mark_flushed(pending[0].id).await.unwrap());
        let pending = outbox.pending(10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].origin_seq, 2);
    }
}
