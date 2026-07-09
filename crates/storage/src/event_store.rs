//! SQLite-backed [`EventStore`] implementation.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use daruma_domain::Actor;
use daruma_events::{Event, EventEnvelope, EventStore};
use daruma_shared::{CoreError, DeviceId, EventId, Result};
use sqlx::{QueryBuilder, Row, SqlitePool};

/// SQLite-backed event log.
///
/// Wraps a [`SqlitePool`]. Construct from [`crate::Db::pool()`] after
/// calling [`crate::Db::migrate()`].
pub struct SqliteEventStore {
    pub(crate) pool: SqlitePool,
}

impl SqliteEventStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl EventStore for SqliteEventStore {
    async fn append(&self, envelope: EventEnvelope) -> Result<EventEnvelope> {
        let event_id = envelope.id.to_string();
        let occurred_at = envelope.occurred_at.to_rfc3339();
        let kind = envelope.kind();
        let actor_json =
            serde_json::to_string(&envelope.actor).map_err(|e| CoreError::serde(e.to_string()))?;
        let payload_json = serde_json::to_string(&envelope.payload)
            .map_err(|e| CoreError::serde(e.to_string()))?;
        let origin_device_id = envelope.origin_device_id.map(|id| id.to_string());

        let result = sqlx::query(
            "INSERT INTO events \
             (event_id, occurred_at, kind, actor_json, payload_json, origin_device_id, origin_seq) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&event_id)
        .bind(&occurred_at)
        .bind(kind)
        .bind(&actor_json)
        .bind(&payload_json)
        .bind(origin_device_id)
        .bind(envelope.origin_seq as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        let seq = result.last_insert_rowid() as u64;
        Ok(EventEnvelope { seq, ..envelope })
    }

    async fn append_batch(&self, envelopes: Vec<EventEnvelope>) -> Result<Vec<EventEnvelope>> {
        if envelopes.is_empty() {
            return Ok(vec![]);
        }

        // Serialize all payloads upfront so serde errors surface before touching the DB.
        struct Entry {
            env: EventEnvelope,
            event_id: String,
            occurred_at: String,
            kind: &'static str,
            actor_json: String,
            payload_json: String,
            origin_device_id: Option<String>,
            origin_seq: u64,
        }

        let mut entries = Vec::with_capacity(envelopes.len());
        for env in envelopes {
            let actor_json =
                serde_json::to_string(&env.actor).map_err(|e| CoreError::serde(e.to_string()))?;
            let payload_json =
                serde_json::to_string(&env.payload).map_err(|e| CoreError::serde(e.to_string()))?;
            entries.push(Entry {
                event_id: env.id.to_string(),
                occurred_at: env.occurred_at.to_rfc3339(),
                kind: env.kind(),
                actor_json,
                payload_json,
                origin_device_id: env.origin_device_id.map(|id| id.to_string()),
                origin_seq: env.origin_seq,
                env,
            });
        }

        // 7 bind params per row; chunk ≤ 3 000 rows keeps us under
        // SQLITE_MAX_VARIABLE_NUMBER (32 766 ÷ 7 = 4 680 max).
        // Each chunk becomes a single  INSERT … VALUES (…),(…),…  statement,
        // avoiding the per-statement parse/plan overhead of the old row-at-a-time
        // loop.  SQLite assigns consecutive rowids within a single INSERT on a
        // BEGIN IMMEDIATE connection, so we can recover individual seqs from
        // `last_insert_rowid()` alone.
        const CHUNK: usize = 3_000;

        // Acquire a single connection and use BEGIN IMMEDIATE to serialise writers.
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *conn)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        let mut out = Vec::with_capacity(entries.len());
        let mut entry_iter = entries.into_iter().peekable();

        while entry_iter.peek().is_some() {
            let chunk: Vec<Entry> = entry_iter.by_ref().take(CHUNK).collect();
            let chunk_len = chunk.len();

            // Build one INSERT … VALUES (…),(…),… for the whole chunk.
            // We clone the String fields so `chunk` stays fully owned and can
            // be consumed below to assign seqs — binding by reference would tie
            // the builder's lifetime to `chunk`, blocking the subsequent move.
            let mut builder = QueryBuilder::<sqlx::Sqlite>::new(
                "INSERT INTO events \
                 (event_id, occurred_at, kind, actor_json, payload_json, origin_device_id, origin_seq) ",
            );
            builder.push_values(chunk.iter(), |mut b, e| {
                b.push_bind(e.event_id.clone())
                    .push_bind(e.occurred_at.clone())
                    .push_bind(e.kind) // &'static str — no clone needed
                    .push_bind(e.actor_json.clone())
                    .push_bind(e.payload_json.clone())
                    .push_bind(e.origin_device_id.clone())
                    .push_bind(e.origin_seq as i64);
            });

            match builder.build().execute(&mut *conn).await {
                Ok(r) => {
                    let last_rowid = r.last_insert_rowid() as u64;
                    // SQLite assigns consecutive rowids for a single INSERT VALUES
                    // statement; first row = last_rowid - chunk_len + 1.
                    let first_rowid = last_rowid - chunk_len as u64 + 1;
                    for (i, entry) in chunk.into_iter().enumerate() {
                        out.push(EventEnvelope {
                            seq: first_rowid + i as u64,
                            ..entry.env
                        });
                    }
                }
                Err(e) => {
                    let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                    return Err(CoreError::storage(e.to_string()));
                }
            }
        }

        sqlx::query("COMMIT")
            .execute(&mut *conn)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(out)
    }

    async fn load_since(&self, since_seq: u64, limit: usize) -> Result<Vec<EventEnvelope>> {
        let rows = sqlx::query(
            "SELECT seq, event_id, occurred_at, actor_json, payload_json, \
             origin_device_id, origin_seq \
             FROM events WHERE seq > ? ORDER BY seq ASC LIMIT ?",
        )
        .bind(since_seq as i64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(parse_row).collect()
    }

    async fn load_by_id(&self, id: EventId) -> Result<Option<EventEnvelope>> {
        let row = sqlx::query(
            "SELECT seq, event_id, occurred_at, actor_json, payload_json, \
             origin_device_id, origin_seq \
             FROM events WHERE event_id = ? LIMIT 1",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(parse_row).transpose()
    }

    async fn latest_seq(&self) -> Result<u64> {
        let row = sqlx::query("SELECT COALESCE(MAX(seq), 0) AS max_seq FROM events")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        let seq: i64 = row
            .try_get("max_seq")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(seq as u64)
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn parse_row(row: &sqlx::sqlite::SqliteRow) -> Result<EventEnvelope> {
    let seq: i64 = row
        .try_get("seq")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let event_id: String = row
        .try_get("event_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let occurred_at_s: String = row
        .try_get("occurred_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let actor_json: String = row
        .try_get("actor_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let payload_json: String = row
        .try_get("payload_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let origin_device_id_s: Option<String> = row
        .try_get("origin_device_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let origin_seq: i64 = row
        .try_get("origin_seq")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    let id: EventId = event_id
        .parse()
        .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?;
    let occurred_at: DateTime<Utc> = DateTime::parse_from_rfc3339(&occurred_at_s)
        .map_err(|e| CoreError::serde(e.to_string()))?
        .with_timezone(&Utc);
    let actor: Actor =
        serde_json::from_str(&actor_json).map_err(|e| CoreError::serde(e.to_string()))?;
    let payload: Event =
        serde_json::from_str(&payload_json).map_err(|e| CoreError::serde(e.to_string()))?;

    Ok(EventEnvelope {
        id,
        seq: seq as u64,
        occurred_at,
        actor,
        origin_device_id: origin_device_id_s
            .map(|id| id.parse::<DeviceId>())
            .transpose()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        origin_seq: origin_seq as u64,
        payload,
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_domain::{Actor, NewTask};
    use daruma_events::Event;
    use daruma_shared::DeviceId;

    #[tokio::test]
    async fn round_trip_append_load_since() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let store = SqliteEventStore::new(db.pool().clone());

        let task = NewTask::new("test round-trip");
        let payload = Event::TaskCreated { task };
        let envelope = EventEnvelope::new(Actor::user(), payload);
        let original_id = envelope.id;

        // append assigns seq > 0
        let saved = store.append(envelope).await.unwrap();
        assert!(saved.seq > 0, "seq should be assigned by the store");
        assert_eq!(saved.id, original_id, "id must be preserved");

        // load_since(0) returns the same envelope
        let loaded = store.load_since(0, 100).await.unwrap();
        assert_eq!(loaded.len(), 1);
        let e = &loaded[0];
        assert_eq!(e.id, original_id);
        assert_eq!(e.seq, saved.seq);

        // latest_seq matches
        let latest = store.latest_seq().await.unwrap();
        assert_eq!(latest, saved.seq);

        // load_since(seq) returns empty
        let after = store.load_since(saved.seq, 100).await.unwrap();
        assert!(after.is_empty());
    }

    #[tokio::test]
    async fn load_by_id_returns_matching_event() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let store = SqliteEventStore::new(db.pool().clone());

        let saved = store
            .append(EventEnvelope::new(
                Actor::user(),
                Event::TaskCreated {
                    task: NewTask::new("by id"),
                },
            ))
            .await
            .unwrap();

        let loaded = store.load_by_id(saved.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, saved.id);
        assert_eq!(loaded.seq, saved.seq);
    }

    #[tokio::test]
    async fn append_batch_begin_immediate() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let store = SqliteEventStore::new(db.pool().clone());

        let batch: Vec<EventEnvelope> = (0..3)
            .map(|i| {
                let task = NewTask::new(format!("task {i}"));
                EventEnvelope::new(Actor::user(), Event::TaskCreated { task })
            })
            .collect();

        let saved = store.append_batch(batch).await.unwrap();
        assert_eq!(saved.len(), 3);
        // seqs are strictly monotonic
        assert!(saved[0].seq < saved[1].seq);
        assert!(saved[1].seq < saved[2].seq);
    }

    #[tokio::test]
    async fn round_trip_origin_device_metadata() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let store = SqliteEventStore::new(db.pool().clone());

        let device_id = DeviceId::new();
        let mut envelope = EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated {
                task: NewTask::new("device event"),
            },
        );
        envelope.origin_device_id = Some(device_id);
        envelope.origin_seq = 42;

        let saved = store.append(envelope).await.unwrap();
        let loaded = store.load_since(0, 100).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, saved.id);
        assert_eq!(loaded[0].origin_device_id, Some(device_id));
        assert_eq!(loaded[0].origin_seq, 42);
    }
}
