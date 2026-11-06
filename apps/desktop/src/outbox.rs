//! Local outbox for commands/events created while the desktop is offline.

#![allow(dead_code)] // Wired into local_executor/reconnect flush in the next Phase 2 block.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use daruma_core::embed::{Db, EventEnvelope, EventStore, SqliteEventStore};
use daruma_domain::Actor;
use daruma_shared::{CoreError, DeviceId, EventId, Result, TaskId};
use sqlx::{Sqlite, SqliteExecutor, SqlitePool, Transaction};

use crate::replica::ReplicaSource;

/// Events of a local command that reached the log but maybe not the
/// projectors. The catch-up rebuilds the projections when it finds any.
pub const UNPROJECTED_SCHEMA: &str =
    "CREATE TABLE IF NOT EXISTS desktop_unprojected (event_id TEXT PRIMARY KEY)";

/// Events of a local command the server has not yet put in its order. They
/// wait for their echo, and nothing can have overtaken a later one through them.
pub const UNECHOED_SCHEMA: &str =
    "CREATE TABLE IF NOT EXISTS desktop_unechoed (event_id TEXT PRIMARY KEY)";

/// Every kind the commands of an older binary could log. Newer events are
/// queued with their commit, so `find_lost` never needs to grow this.
const OLD_BINARY_KINDS: [&str; 5] = [
    "task_created",
    "task_status_changed",
    "task_completed",
    "task_closed",
    "task_deleted",
];

/// An event an older binary may have lost, until someone says it may go out.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnconfirmedEvent {
    pub id: EventId,
    pub kind: String,
}

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
        // Dead letters: events the server refused for good (a final 4xx).
        // They stay in `desktop_outbox` for inspection but leave the queue.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS desktop_outbox_rejected (
                outbox_id INTEGER PRIMARY KEY REFERENCES desktop_outbox(id),
                status INTEGER NOT NULL,
                code TEXT NOT NULL,
                rejected_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
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
        for schema in [UNPROJECTED_SCHEMA, UNECHOED_SCHEMA] {
            sqlx::query(schema)
                .execute(self.db.pool())
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
        }
        // The origin of this replica. It outlives the process, so a retried
        // event keeps its identity. log_checked: 1 once the log was searched
        // for what an older binary left out of the queue.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS desktop_device (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                device_id TEXT NOT NULL,
                log_checked INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(self.db.pool())
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        sqlx::query("INSERT OR IGNORE INTO desktop_device (id, device_id) VALUES (1, ?)")
            .bind(DeviceId::new().to_string())
            .execute(self.db.pool())
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        sqlx::query("CREATE TABLE IF NOT EXISTS desktop_unconfirmed (event_id TEXT PRIMARY KEY)")
            .execute(self.db.pool())
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// The store local commands must write through.
    pub async fn outbound_store(&self) -> Result<OutboundStore> {
        Ok(OutboundStore {
            inner: SqliteEventStore::new(self.db.pool().clone()),
            pool: self.db.pool().clone(),
            origin_device_id: self.device_id().await?,
        })
    }

    async fn device_id(&self) -> Result<DeviceId> {
        let (device_id,) =
            sqlx::query_as::<_, (String,)>("SELECT device_id FROM desktop_device WHERE id = 1")
                .fetch_one(self.db.pool())
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
        device_id
            .parse::<DeviceId>()
            .map_err(|e| CoreError::serde(e.to_string()))
    }

    pub async fn enqueue(
        &self,
        origin_device_id: DeviceId,
        origin_seq: u64,
        envelope: EventEnvelope,
    ) -> Result<bool> {
        enqueue_on(self.db.pool(), origin_device_id, origin_seq, envelope).await
    }

    /// Pending events in the order of the local log, which is the order they
    /// were made in, whichever way they got into the queue.
    pub async fn pending(&self, limit: u32) -> Result<Vec<OutboxEntry>> {
        let rows = sqlx::query_as::<_, OutboxRow>(
            "SELECT id, origin_device_id, origin_seq, envelope_json
             FROM desktop_outbox
             WHERE flushed_at IS NULL
               AND id NOT IN (SELECT outbox_id FROM desktop_outbox_rejected)
             ORDER BY json_extract(envelope_json, '$.seq'), origin_device_id, origin_seq
             LIMIT ?",
        )
        .bind(limit.max(1) as i64)
        .fetch_all(self.db.pool())
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.into_iter().map(OutboxEntry::try_from).collect()
    }

    /// Pending events that may go out now. What follows a held event of the
    /// same task stays behind it: a server that takes a change of a task it has
    /// never seen drops it, and the create that comes later starts the task over.
    pub async fn sendable(&self, limit: u32) -> Result<Vec<OutboxEntry>> {
        let held = self.held_tasks().await?;
        if held.is_empty() {
            return self.pending(limit).await;
        }
        let pending = self.pending(u32::MAX).await?;
        Ok(pending
            .into_iter()
            .filter(|entry| !waits_for_held(&held, entry))
            .take(limit.max(1) as usize)
            .collect())
    }

    /// How many pending events wait behind a held one.
    pub async fn held_back(&self) -> Result<usize> {
        let held = self.held_tasks().await?;
        let pending = self.pending(u32::MAX).await?;
        Ok(pending
            .iter()
            .filter(|entry| waits_for_held(&held, entry))
            .count())
    }

    /// Where the first held event of each task sits in the log.
    async fn held_tasks(&self) -> Result<HashMap<TaskId, u64>> {
        let store = SqliteEventStore::new(self.db.pool().clone());
        let mut held = HashMap::new();
        for unconfirmed in self.unconfirmed().await? {
            let event = store
                .load_by_id(unconfirmed.id)
                .await?
                .ok_or_else(|| CoreError::storage(format!("{} left the log", unconfirmed.id)))?;
            if let Some(task) = event.payload.target_task() {
                held.entry(task).or_insert(event.seq);
            }
        }
        Ok(held)
    }

    /// The command that wrote these events got past its projectors.
    pub async fn mark_projected(&self, events: &[EventEnvelope]) -> Result<()> {
        for event in events {
            sqlx::query("DELETE FROM desktop_unprojected WHERE event_id = ?")
                .bind(event.id.to_string())
                .execute(self.db.pool())
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
        }
        Ok(())
    }

    /// Search the log, once per replica, for what an older binary logged and
    /// never queued: it committed a command first and queued its events
    /// afterwards, so a stop or a storage error in between left them on this
    /// device alone. An incoming event of a server this replica has left reads
    /// the same in every column, and the log of the current server says nothing
    /// about it, so a match only goes on record: [`Outbox::restore_lost`] is the
    /// way out. What the server already has is delivered or incoming, and
    /// whatever came from it before `local_tip` was read is in the log read
    /// after it.
    pub async fn find_lost(&self, remote: &dyn ReplicaSource, limit: u32) -> Result<()> {
        let pool = self.db.pool();
        let (checked, local_tip, cursor) = sqlx::query_as::<_, (i64, i64, i64)>(
            "SELECT log_checked, (SELECT COALESCE(MAX(seq), 0) FROM events),
                    (SELECT server_seq FROM desktop_replica_state WHERE id = 1)
             FROM desktop_device WHERE id = 1",
        )
        .fetch_one(pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        if checked != 0 {
            return Ok(());
        }
        let mut server_ids = HashSet::new();
        if !lost_candidates(pool, local_tip).await?.is_empty() {
            server_ids = server_event_ids(remote, limit, cursor).await?;
        }

        let mut tx = begin_write(pool).await?;
        for event_id in lost_candidates(&mut *tx, local_tip).await? {
            if server_ids.contains(&event_id) {
                continue;
            }
            sqlx::query("INSERT OR IGNORE INTO desktop_unconfirmed (event_id) VALUES (?)")
                .bind(event_id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
        }
        sqlx::query("UPDATE desktop_device SET log_checked = 1 WHERE id = 1")
            .execute(&mut *tx)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))
    }

    /// What [`Outbox::find_lost`] left waiting for a decision, in log order.
    pub async fn unconfirmed(&self) -> Result<Vec<UnconfirmedEvent>> {
        unconfirmed_on(self.db.pool()).await
    }

    /// Queue the events on record, on the word of a user who knows this replica
    /// has only ever synced with this server: nothing local can prove it, and a
    /// wrong answer publishes another server's events here. Those this server
    /// has by now stay off the queue. Returns how many were queued.
    pub async fn restore_lost(&self, remote: &dyn ReplicaSource, limit: u32) -> Result<usize> {
        let pool = self.db.pool();
        if self.unconfirmed().await?.is_empty() {
            return Ok(0);
        }
        let (cursor,) = sqlx::query_as::<_, (i64,)>(
            "SELECT server_seq FROM desktop_replica_state WHERE id = 1",
        )
        .fetch_one(pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        let server_ids = server_event_ids(remote, limit, cursor).await?;

        let origin_device_id = self.device_id().await?;
        let mut tx = begin_write(pool).await?;
        let mut restored = 0;
        for unconfirmed in unconfirmed_on(&mut *tx).await? {
            if server_ids.contains(&unconfirmed.id) {
                continue;
            }
            let event = SqliteEventStore::load_by_id_tx(&mut tx, unconfirmed.id)
                .await?
                .ok_or_else(|| CoreError::storage(format!("{} left the log", unconfirmed.id)))?;
            enqueue_next(&mut tx, origin_device_id, event).await?;
            restored += 1;
        }
        sqlx::query("DELETE FROM desktop_unconfirmed")
            .execute(&mut *tx)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(restored)
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

    /// Move an entry to the dead letters: the server refused it for good
    /// (HTTP `status`, cloud error `code`), so it no longer blocks the queue.
    pub async fn mark_rejected(&self, id: i64, status: u16, code: &str) -> Result<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO desktop_outbox_rejected (outbox_id, status, code)
             VALUES (?, ?, ?)",
        )
        .bind(id)
        .bind(status as i64)
        .bind(code)
        .execute(self.db.pool())
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Return every dead letter to the queue; how many came back.
    pub async fn requeue_rejected(&self) -> Result<u64> {
        let res = sqlx::query("DELETE FROM desktop_outbox_rejected")
            .execute(self.db.pool())
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(res.rows_affected())
    }

    /// How many events the server has refused so far.
    pub async fn rejected_count(&self) -> Result<u64> {
        let row = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM desktop_outbox_rejected")
            .fetch_one(self.db.pool())
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(row.0.max(0) as u64)
    }
}

/// [`EventStore`] of local commands: an event and its outbox entry commit
/// together, so no stop can leave a local change behind with nothing to carry
/// it to the server. Incoming events bypass it: they append in `Replica`.
pub struct OutboundStore {
    inner: SqliteEventStore,
    pool: SqlitePool,
    origin_device_id: DeviceId,
}

#[async_trait]
impl EventStore for OutboundStore {
    async fn append(&self, envelope: EventEnvelope) -> Result<EventEnvelope> {
        let mut persisted = self.append_batch(vec![envelope]).await?;
        persisted
            .pop()
            .ok_or_else(|| CoreError::storage("append returned no event"))
    }

    async fn append_batch(&self, envelopes: Vec<EventEnvelope>) -> Result<Vec<EventEnvelope>> {
        let mut persisted = Vec::with_capacity(envelopes.len());
        let mut tx = begin_write(&self.pool).await?;
        for envelope in envelopes {
            let event = SqliteEventStore::append_tx(&mut tx, envelope).await?;
            enqueue_next(&mut tx, self.origin_device_id, event.clone()).await?;
            for table in ["desktop_unprojected", "desktop_unechoed"] {
                sqlx::query(&format!("INSERT INTO {table} (event_id) VALUES (?)"))
                    .bind(event.id.to_string())
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }
            persisted.push(event);
        }
        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(persisted)
    }

    async fn load_since(&self, since_seq: u64, limit: usize) -> Result<Vec<EventEnvelope>> {
        self.inner.load_since(since_seq, limit).await
    }

    async fn load_by_id(&self, id: EventId) -> Result<Option<EventEnvelope>> {
        self.inner.load_by_id(id).await
    }

    async fn latest_seq(&self) -> Result<u64> {
        self.inner.latest_seq().await
    }
}

/// A transaction that holds the write lock from its first statement, so the
/// next `origin_seq` it reads stays free until it commits.
async fn begin_write(pool: &SqlitePool) -> Result<Transaction<'_, Sqlite>> {
    pool.begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(|e| CoreError::storage(e.to_string()))
}

pub(crate) async fn enqueue_on<'e, E: SqliteExecutor<'e>>(
    executor: E,
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
    .execute(executor)
    .await
    .map_err(|e| CoreError::storage(e.to_string()))?;
    Ok(res.rows_affected() > 0)
}

/// Queue `envelope` behind everything this device has queued.
async fn enqueue_next(
    tx: &mut Transaction<'_, Sqlite>,
    origin_device_id: DeviceId,
    envelope: EventEnvelope,
) -> Result<()> {
    let (last,) = sqlx::query_as::<_, (Option<i64>,)>(
        "SELECT MAX(origin_seq) FROM desktop_outbox WHERE origin_device_id = ?",
    )
    .bind(origin_device_id.to_string())
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| CoreError::storage(e.to_string()))?;
    let origin_seq = last.unwrap_or(0).max(0) as u64 + 1;
    if enqueue_on(&mut **tx, origin_device_id, origin_seq, envelope).await? {
        Ok(())
    } else {
        Err(CoreError::storage(format!(
            "outbox already holds {origin_device_id} seq {origin_seq}"
        )))
    }
}

fn waits_for_held(held: &HashMap<TaskId, u64>, entry: &OutboxEntry) -> bool {
    let task = entry.envelope.payload.target_task();
    task.and_then(|task| held.get(&task))
        .is_some_and(|first| entry.envelope.seq > *first)
}

/// Log events up to `local_tip` no queue entry covers that a command of this
/// device can have made: incoming events keep their origin, and commands of
/// this device run as the anonymous local user.
async fn lost_candidates<'e, E: SqliteExecutor<'e>>(
    executor: E,
    local_tip: i64,
) -> Result<Vec<EventId>> {
    let actor =
        serde_json::to_string(&Actor::user()).map_err(|e| CoreError::serde(e.to_string()))?;
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT event_id FROM events
         WHERE seq <= ?
           AND origin_device_id IS NULL
           AND actor_json = ?
           AND kind IN (SELECT value FROM json_each(?))
           AND event_id NOT IN (
               SELECT 'evt_' || json_extract(envelope_json, '$.id') FROM desktop_outbox
           )
         ORDER BY seq",
    )
    .bind(local_tip)
    .bind(actor)
    .bind(serde_json::json!(OLD_BINARY_KINDS).to_string())
    .fetch_all(executor)
    .await
    .map_err(|e| CoreError::storage(e.to_string()))?;
    rows.into_iter()
        .map(|(id,)| {
            id.parse::<EventId>()
                .map_err(|e| CoreError::serde(e.to_string()))
        })
        .collect()
}

async fn unconfirmed_on<'e, E: SqliteExecutor<'e>>(executor: E) -> Result<Vec<UnconfirmedEvent>> {
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT events.event_id, events.kind FROM desktop_unconfirmed
         JOIN events ON events.event_id = desktop_unconfirmed.event_id
         ORDER BY events.seq",
    )
    .fetch_all(executor)
    .await
    .map_err(|e| CoreError::storage(e.to_string()))?;
    rows.into_iter()
        .map(|(id, kind)| {
            let id = id
                .parse::<EventId>()
                .map_err(|e| CoreError::serde(e.to_string()))?;
            Ok(UnconfirmedEvent { id, kind })
        })
        .collect()
}

async fn server_event_ids(
    remote: &dyn ReplicaSource,
    limit: u32,
    cursor: i64,
) -> Result<HashSet<EventId>> {
    let mut ids = HashSet::new();
    let mut since = 0;
    loop {
        let page = remote.fetch_events(since, limit).await?;
        let Some(last) = page.last() else {
            break;
        };
        since = last.seq;
        ids.extend(page.iter().map(|event| event.id));
    }
    if cursor > 0 && ids.is_empty() {
        // An empty log cannot be the one this cursor came from, and would
        // pass everything incoming off as lost.
        return Err(CoreError::sync(format!(
            "server log is empty but the replica cursor is at {cursor}"
        )));
    }
    Ok(ids)
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
    use daruma_core::embed::{Event, EventEnvelope};
    use daruma_domain::{Actor, NewTask};

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

    #[tokio::test]
    async fn requeue_rejected_returns_dead_letters_to_pending() {
        let outbox = Outbox::new(Db::memory().await.unwrap());
        outbox.ensure_schema().await.unwrap();
        let envelope = EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated {
                task: NewTask::new("refused"),
            },
        );
        outbox.enqueue(DeviceId::new(), 1, envelope).await.unwrap();
        let id = outbox.pending(10).await.unwrap()[0].id;
        outbox
            .mark_rejected(id, 403, "intake_source_admin_only")
            .await
            .unwrap();
        assert!(outbox.pending(10).await.unwrap().is_empty());

        assert_eq!(outbox.requeue_rejected().await.unwrap(), 1);
        assert_eq!(outbox.pending(10).await.unwrap().len(), 1);
        assert_eq!(outbox.rejected_count().await.unwrap(), 0);
        assert_eq!(outbox.requeue_rejected().await.unwrap(), 0);
    }
}
