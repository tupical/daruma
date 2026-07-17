//! Bootstrap snapshot store — periodic materialised projection state with a
//! seq mark, used to speed up device-sync catch-up (§3.3 Phase 5 follow-up).
//!
//! A freshly paired device used to replay the whole event log from seq 0
//! (`EventStore::load_since`). On a large workspace that is expensive, so
//! the server periodically snapshots the write-through projections a device
//! replica maintains (tasks / projects / comments) and labels the snapshot
//! with the event-log seq it was taken at. A new device restores the latest
//! snapshot and replays only the delta (`load_since(snapshot.seq)`).
//!
//! Projectors are upsert-style (INSERT OR REPLACE keyed by id), so the seq
//! label does not need to be a perfectly consistent cut: a snapshot taken at
//! `seq` may already reflect a few newer events, and re-applying those
//! events from the delta is a no-op. The delta always covers `(seq, ∞)`, so
//! the device converges either way.

use chrono::{DateTime, Utc};
use daruma_domain::{Comment, Project, Task};
use daruma_shared::{CoreError, Result};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::parse_ts;

/// Materialised state of the device-replicated projections at one point of
/// the event log. This is the payload shipped to catching-up devices; keep
/// it limited to what the desktop replica applies (`replica.rs`).
///
/// Derived/denormalised side tables are deliberately excluded: the
/// `activity` feed is history (as large as the log itself) and rebuilds
/// from the delta going forward; `entity_versions`, `status_changed_at` and
/// project slug aliases are server-side audit/lookup aids the read
/// projections do not depend on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectionSnapshot {
    pub tasks: Vec<Task>,
    pub projects: Vec<Project>,
    pub comments: Vec<Comment>,
}

/// A persisted snapshot: seq mark + creation time + projection payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub seq: u64,
    pub created_at: DateTime<Utc>,
    pub payload: ProjectionSnapshot,
}

/// Read/write access to the `snapshots` table (migration 0051).
pub struct SnapshotRepo {
    pool: SqlitePool,
}

impl SnapshotRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Newest snapshot by seq, if any has been written yet.
    pub async fn latest(&self) -> Result<Option<Snapshot>> {
        let row = sqlx::query(
            "SELECT id, seq, created_at, payload_json FROM snapshots \
             ORDER BY seq DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(row_to_snapshot).transpose()
    }

    /// Persist a snapshot of `payload` taken at event-log seq `seq`.
    pub async fn insert(&self, seq: u64, payload: &ProjectionSnapshot) -> Result<Snapshot> {
        let snapshot = Snapshot {
            id: uuid::Uuid::new_v4().to_string(),
            seq,
            created_at: Utc::now(),
            payload: payload.clone(),
        };
        let payload_json =
            serde_json::to_string(payload).map_err(|e| CoreError::serde(e.to_string()))?;

        sqlx::query(
            "INSERT INTO snapshots (id, seq, created_at, payload_json) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&snapshot.id)
        .bind(seq as i64)
        .bind(snapshot.created_at.to_rfc3339())
        .bind(payload_json)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(snapshot)
    }

    /// Drop all but the `keep` newest snapshots. Returns rows removed.
    pub async fn prune_keep_latest(&self, keep: u32) -> Result<u64> {
        let removed = sqlx::query(
            "DELETE FROM snapshots WHERE id NOT IN \
             (SELECT id FROM snapshots ORDER BY seq DESC LIMIT ?)",
        )
        .bind(keep as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?
        .rows_affected();
        Ok(removed)
    }
}

// ── row mapping ───────────────────────────────────────────────────────────────

fn row_to_snapshot(row: &sqlx::sqlite::SqliteRow) -> Result<Snapshot> {
    let id: String = row
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let seq: i64 = row
        .try_get("seq")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let created_at_s: String = row
        .try_get("created_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let payload_json: String = row
        .try_get("payload_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    let payload: ProjectionSnapshot =
        serde_json::from_str(&payload_json).map_err(|e| CoreError::serde(e.to_string()))?;

    Ok(Snapshot {
        id,
        seq: seq as u64,
        created_at: parse_ts(&created_at_s)?,
        payload,
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_domain::NewTask;

    fn payload_with_task(title: &str) -> ProjectionSnapshot {
        ProjectionSnapshot {
            tasks: vec![Task::from_new(NewTask::new(title))],
            projects: vec![],
            comments: vec![],
        }
    }

    #[tokio::test]
    async fn insert_then_latest_round_trips() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = SnapshotRepo::new(db.pool().clone());

        assert!(repo.latest().await.unwrap().is_none());

        let payload = payload_with_task("snapshotted");
        let written = repo.insert(42, &payload).await.unwrap();
        assert_eq!(written.seq, 42);

        let latest = repo.latest().await.unwrap().expect("snapshot exists");
        assert_eq!(latest.id, written.id);
        assert_eq!(latest.seq, 42);
        assert_eq!(latest.payload, payload);
    }

    #[tokio::test]
    async fn latest_returns_highest_seq_and_prune_keeps_newest() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = SnapshotRepo::new(db.pool().clone());

        repo.insert(10, &ProjectionSnapshot::default())
            .await
            .unwrap();
        repo.insert(30, &ProjectionSnapshot::default())
            .await
            .unwrap();
        repo.insert(20, &ProjectionSnapshot::default())
            .await
            .unwrap();

        assert_eq!(repo.latest().await.unwrap().unwrap().seq, 30);

        let removed = repo.prune_keep_latest(2).await.unwrap();
        assert_eq!(removed, 1);
        let latest = repo.latest().await.unwrap().unwrap();
        assert_eq!(latest.seq, 30);

        let removed = repo.prune_keep_latest(1).await.unwrap();
        assert_eq!(removed, 1);
        assert_eq!(repo.latest().await.unwrap().unwrap().seq, 30);
    }
}
