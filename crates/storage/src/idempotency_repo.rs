//! IdempotencyRepo — processed_command_ids table for Linear A.1.
//!
//! Clients supply a `client_command_id: Uuid` with mutation requests; we store
//! the resulting `(server_event_id, server_event_seq)` so repeated requests
//! return the cached result without re-executing the command.
//!
//! TTL: rows older than 7 days are swept by a background task in the server.
//!
//! ## Bloom-filter fast path
//!
//! A [`fastbloom::BloomFilter`] guards the `lookup` hot path.  On a definite
//! miss (filter says "not present") we skip the SQL round-trip entirely; on a
//! possible hit (~1 % false-positive rate) we confirm with a DB query.
//!
//! The filter starts empty; call [`IdempotencyRepo::warm`] once at server
//! startup to seed it from existing rows.  Tests can skip `warm` — the filter
//! fills naturally as commands are processed.

use chrono::{DateTime, Duration, Utc};
use fastbloom::BloomFilter;
use sqlx::{Row, SqlitePool};
use std::sync::RwLock;
use daruma_shared::{CoreError, EventId, Result};
use uuid::Uuid;

fn reserved_event_id() -> EventId {
    EventId::from_uuid(Uuid::nil())
}

/// Read/write access to the `processed_command_ids` table.
pub struct IdempotencyRepo {
    pub(crate) pool: SqlitePool,
    /// In-memory bloom filter for fast-path rejection of unseen command IDs.
    ///
    /// Sized for 100 000 entries at ≈1 % false-positive rate (≈ 959 KiB of
    /// bits), which covers roughly one day's expected command volume.  A read
    /// lock is held only for the duration of the bit-check; a write lock only
    /// for the bit-set on insert.
    filter: RwLock<BloomFilter>,
}

impl IdempotencyRepo {
    /// Construct a repo with an empty bloom filter.
    ///
    /// Call [`Self::warm`] once after construction to seed from existing rows.
    pub fn new(pool: SqlitePool) -> Self {
        // 100 000 entries × 1 % FPR → ≈959 KiB.
        let filter = BloomFilter::with_false_pos(0.01).expected_items(100_000);
        Self {
            pool,
            filter: RwLock::new(filter),
        }
    }

    /// Seed the bloom filter from all rows currently in `processed_command_ids`.
    ///
    /// Call once at server startup after pool construction.  Safe to skip in
    /// tests — the filter fills naturally on the first inserts.  DB I/O
    /// completes before the write-lock is acquired so callers are not blocked
    /// during the query.
    pub async fn warm(&self) -> Result<()> {
        // Load IDs before acquiring the write lock to avoid blocking lookups
        // during the DB round-trip.
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT client_command_id FROM processed_command_ids")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;

        let mut filter = self.filter.write().expect("bloom filter RwLock poisoned");
        for (id_str,) in rows {
            if let Ok(uuid) = Uuid::parse_str(&id_str) {
                filter.insert(&uuid);
            }
        }
        Ok(())
    }

    // ── queries ──────────────────────────────────────────────────────────────

    /// Look up a previously processed command.
    ///
    /// Returns `Some((server_event_id, server_event_seq))` on a cache hit,
    /// `None` on a miss.
    ///
    /// A bloom-filter pre-check short-circuits the SQL query for definite
    /// misses.  False positives (~1 %) still reach the DB and return `None`.
    pub async fn lookup(&self, client_command_id: Uuid) -> Result<Option<(EventId, u64)>> {
        // Fast path: skip the DB round-trip for IDs we have never seen.
        {
            let filter = self.filter.read().expect("bloom filter RwLock poisoned");
            if !filter.contains(&client_command_id) {
                return Ok(None);
            }
        }

        let row = sqlx::query(
            "SELECT server_event_id, server_event_seq \
             FROM processed_command_ids WHERE client_command_id = ?",
        )
        .bind(client_command_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        match row {
            None => Ok(None),
            Some(r) => {
                let event_id_s: String = r
                    .try_get("server_event_id")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let event_seq: i64 = r
                    .try_get("server_event_seq")
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                let event_id = event_id_s
                    .parse::<EventId>()
                    .map_err(|e| CoreError::serde(e.to_string()))?;
                if event_seq <= 0 || event_id == reserved_event_id() {
                    Ok(None)
                } else {
                    Ok(Some((event_id, event_seq as u64)))
                }
            }
        }
    }

    pub async fn lookup_event_id(
        &self,
        client_event_id: EventId,
    ) -> Result<Option<(EventId, u64)>> {
        self.lookup(client_event_id.as_uuid()).await
    }

    // ── mutations ────────────────────────────────────────────────────────────

    /// Record a processed command.  Silently ignores duplicate inserts (best-effort).
    pub async fn insert(
        &self,
        client_command_id: Uuid,
        event_id: EventId,
        event_seq: u64,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT OR IGNORE INTO processed_command_ids \
             (client_command_id, server_event_id, server_event_seq, created_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(client_command_id.to_string())
        .bind(event_id.to_string())
        .bind(event_seq as i64)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        // Mark in the bloom filter so the next lookup for this ID takes the fast path.
        {
            let mut filter = self.filter.write().expect("bloom filter RwLock poisoned");
            filter.insert(&client_command_id);
        }

        Ok(())
    }

    pub async fn reserve_event_id(&self, client_event_id: EventId) -> Result<bool> {
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            "INSERT OR IGNORE INTO processed_command_ids \
             (client_command_id, server_event_id, server_event_seq, created_at) \
             VALUES (?, ?, 0, ?)",
        )
        .bind(client_event_id.as_uuid().to_string())
        .bind(reserved_event_id().to_string())
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        {
            let mut filter = self.filter.write().expect("bloom filter RwLock poisoned");
            filter.insert(&client_event_id.as_uuid());
        }

        Ok(result.rows_affected() > 0)
    }

    pub async fn complete_event_id(
        &self,
        client_event_id: EventId,
        event_id: EventId,
        event_seq: u64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE processed_command_ids \
             SET server_event_id = ?, server_event_seq = ? \
             WHERE client_command_id = ? AND server_event_seq = 0",
        )
        .bind(event_id.to_string())
        .bind(event_seq as i64)
        .bind(client_event_id.as_uuid().to_string())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Delete rows older than `age` and return the number of rows removed.
    pub async fn cleanup_older_than(&self, age: Duration) -> Result<u64> {
        let cutoff = (Utc::now() - age).to_rfc3339();
        let result = sqlx::query("DELETE FROM processed_command_ids WHERE created_at < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(result.rows_affected())
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
    use daruma_shared::EventId;
    use uuid::Uuid;

    async fn make_repo() -> (Db, IdempotencyRepo) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = IdempotencyRepo::new(db.pool().clone());
        (db, repo)
    }

    #[tokio::test]
    async fn idempotency_lookup_miss_returns_none() {
        let (_db, repo) = make_repo().await;
        let result = repo.lookup(Uuid::new_v4()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn idempotency_insert_and_lookup_hit() {
        let (_db, repo) = make_repo().await;
        let ccid = Uuid::new_v4();
        let event_id = EventId::new();
        let event_seq = 42u64;

        repo.insert(ccid, event_id, event_seq).await.unwrap();

        let result = repo.lookup(ccid).await.unwrap();
        assert!(result.is_some());
        let (stored_id, stored_seq) = result.unwrap();
        assert_eq!(stored_id, event_id);
        assert_eq!(stored_seq, event_seq);
    }

    #[tokio::test]
    async fn idempotency_duplicate_insert_is_ignored() {
        let (_db, repo) = make_repo().await;
        let ccid = Uuid::new_v4();
        let event_id = EventId::new();

        repo.insert(ccid, event_id, 1).await.unwrap();
        // Second insert with same ccid must not fail (INSERT OR IGNORE).
        repo.insert(ccid, EventId::new(), 2).await.unwrap();

        let (stored_id, stored_seq) = repo.lookup(ccid).await.unwrap().unwrap();
        // First insert wins.
        assert_eq!(stored_id, event_id);
        assert_eq!(stored_seq, 1);
    }

    #[tokio::test]
    async fn event_id_reservation_blocks_until_completed() {
        let (_db, repo) = make_repo().await;
        let client_event_id = EventId::new();
        let server_event_id = EventId::new();

        assert!(repo.reserve_event_id(client_event_id).await.unwrap());
        assert!(!repo.reserve_event_id(client_event_id).await.unwrap());
        assert!(repo
            .lookup_event_id(client_event_id)
            .await
            .unwrap()
            .is_none());

        repo.complete_event_id(client_event_id, server_event_id, 7)
            .await
            .unwrap();
        let (stored_id, stored_seq) = repo
            .lookup_event_id(client_event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored_id, server_event_id);
        assert_eq!(stored_seq, 7);
    }

    #[tokio::test]
    async fn idempotency_cleanup_older_than() {
        let (db, repo) = make_repo().await;

        // Insert one stale row directly with an old created_at.
        sqlx::query(
            "INSERT INTO processed_command_ids \
             (client_command_id, server_event_id, server_event_seq, created_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(EventId::new().to_string())
        .bind(1i64)
        .bind("2000-01-01T00:00:00+00:00")
        .execute(db.pool())
        .await
        .unwrap();

        // Insert one fresh row via the repo.
        repo.insert(Uuid::new_v4(), EventId::new(), 2)
            .await
            .unwrap();

        // Cleanup rows older than 1 day — should delete the 2000-01-01 row only.
        let deleted = repo.cleanup_older_than(Duration::days(1)).await.unwrap();
        assert_eq!(deleted, 1);

        // The recent row should still exist.
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM processed_command_ids")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(row.0, 1);
    }

    #[tokio::test]
    async fn bloom_warm_seeds_existing_rows() {
        let (db, repo) = make_repo().await;

        // Insert a row directly into the DB (bypassing repo, so bloom is not updated).
        let ccid = Uuid::new_v4();
        let event_id = EventId::new();
        sqlx::query(
            "INSERT INTO processed_command_ids \
             (client_command_id, server_event_id, server_event_seq, created_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(ccid.to_string())
        .bind(event_id.to_string())
        .bind(1i64)
        .bind(Utc::now().to_rfc3339())
        .execute(db.pool())
        .await
        .unwrap();

        // Before warm, bloom doesn't know about the row, so lookup short-circuits.
        // (This verifies the fast-path is active — we can't distinguish DB miss
        // from bloom miss externally, but we can confirm warm makes it findable.)
        repo.warm().await.unwrap();

        // After warm, lookup should return the cached entry.
        let result = repo.lookup(ccid).await.unwrap();
        assert!(result.is_some());
        let (stored_id, _) = result.unwrap();
        assert_eq!(stored_id, event_id);
    }
}
