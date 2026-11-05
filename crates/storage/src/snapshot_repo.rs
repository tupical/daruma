//! Bootstrap snapshot store — periodic materialised projection state with a
//! seq mark, used to speed up device-sync catch-up (§3.3 Phase 5 follow-up).
//!
//! A freshly paired device used to replay the whole event log from seq 0
//! (`EventStore::load_since`). On a large workspace that is expensive, so
//! the server periodically snapshots the state a device replica maintains
//! (tasks / projects / comments) and labels the snapshot with the event-log
//! seq it was taken at. A new device restores the latest snapshot and
//! replays only the delta (`load_since(snapshot.seq)`).
//!
//! The delta starts strictly after `seq`, so `seq` is a promise: every event
//! up to it is in the payload. The live projections cannot keep that promise:
//! `append_batch` commits first and the projectors run afterwards, in other
//! transactions, so a committed event may be unapplied for a moment, or for
//! good after an apply error or a crash. [`ProjectionFold`] therefore derives
//! the payload from the log itself, which is a consistent cut by construction
//! and needs no barrier with the write path.

use chrono::{DateTime, Utc};
use daruma_domain::{Comment, Project, Task};
use daruma_events::{EventEnvelope, EventStore};
use daruma_shared::{CoreError, Result};
use serde::{Deserialize, Serialize};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};

use crate::{
    entity_version::replace_task_versions, parse_ts, CommentRepo, Db, ProjectRepo, TaskRepo,
};

/// Version of the consistency guarantee behind a snapshot. `0` marks a row
/// taken from the live projections, which may miss events up to its seq; such
/// rows are never served or trusted. Bump this only when older snapshots stop
/// being valid.
pub const SNAPSHOT_CUT_VERSION: u32 = 1;

/// Events read from the log per round while folding a snapshot.
const FOLD_PAGE_SIZE: usize = 1000;

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
    /// See [`SNAPSHOT_CUT_VERSION`]. Absent on older servers, which reads as
    /// `0`: a client must not build on that snapshot.
    #[serde(default)]
    pub cut_version: u32,
}

impl Snapshot {
    /// Whether `seq` really covers every event up to it.
    pub fn is_consistent(&self) -> bool {
        self.cut_version == SNAPSHOT_CUT_VERSION
    }
}

/// Replays events through the device-replicated projectors in a throwaway
/// in-memory database, leaving the live one alone.
pub struct ProjectionFold {
    tasks: TaskRepo,
    projects: ProjectRepo,
    comments: CommentRepo,
}

impl ProjectionFold {
    /// Start from `base`, the state at some seq, or from the empty log.
    pub async fn new(base: Option<&ProjectionSnapshot>) -> Result<Self> {
        let db = Db::memory().await?;
        db.migrate().await?;
        let pool = db.pool().clone();
        let fold = Self {
            tasks: TaskRepo::new(pool.clone()),
            projects: ProjectRepo::new(pool.clone()),
            comments: CommentRepo::new(pool),
        };
        if let Some(base) = base {
            // Parents before children, as a replay would produce them.
            for project in &base.projects {
                fold.projects.upsert_project(project).await?;
            }
            for task in &base.tasks {
                fold.tasks.upsert_task(task).await?;
            }
            for comment in &base.comments {
                fold.comments.upsert_comment(comment).await?;
            }
        }
        Ok(fold)
    }

    pub async fn apply(&self, event: &EventEnvelope) -> Result<()> {
        self.tasks.apply_event(event).await?;
        self.projects.apply_event(event).await?;
        self.comments.apply_event(event).await?;
        Ok(())
    }

    /// Make the task history in `tx` the one folded so far. A snapshot leaves
    /// history out, but a repair must not lose the part a stopped sync missed.
    pub async fn replace_task_versions_tx(&self, tx: &mut Transaction<'_, Sqlite>) -> Result<()> {
        replace_task_versions(&self.tasks.pool, tx).await
    }

    /// The projections as folded so far; more events may follow.
    pub async fn state(&self) -> Result<ProjectionSnapshot> {
        Ok(ProjectionSnapshot {
            tasks: self.tasks.list_all().await?,
            projects: self.projects.list_all().await?,
            comments: self.comments.list_all().await?,
        })
    }
}

/// Read/write access to the `snapshots` table (migrations 0051, 0063).
pub struct SnapshotRepo {
    pool: SqlitePool,
}

impl SnapshotRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Newest consistent snapshot by seq, if any has been written yet.
    pub async fn latest(&self) -> Result<Option<Snapshot>> {
        let row = sqlx::query(
            "SELECT id, seq, created_at, payload_json, cut_version FROM snapshots \
             WHERE cut_version = ? ORDER BY seq DESC LIMIT 1",
        )
        .bind(SNAPSHOT_CUT_VERSION as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(row_to_snapshot).transpose()
    }

    /// Fold a new snapshot when at least `min_events` events landed since the
    /// last consistent one, and return its seq. `None` means the threshold has
    /// not been reached. Any event that fails to fold fails the call, so the
    /// served snapshot stays where it was.
    pub async fn write_if_due(
        &self,
        store: &dyn EventStore,
        min_events: u64,
    ) -> Result<Option<u64>> {
        // The log commits in seq order, so every event up to `target` is
        // readable once `target` is.
        let target = store.latest_seq().await?;
        let base = self.latest().await?;
        let mut cursor = base.as_ref().map_or(0, |s| s.seq);
        if target.saturating_sub(cursor) < min_events {
            return Ok(None);
        }

        let fold = ProjectionFold::new(base.as_ref().map(|s| &s.payload)).await?;
        'log: while cursor < target {
            let page = store.load_since(cursor, FOLD_PAGE_SIZE).await?;
            if page.is_empty() {
                // The tail of the log was purged telemetry.
                break;
            }
            for event in &page {
                if event.seq > target {
                    break 'log;
                }
                fold.apply(event).await?;
                cursor = event.seq;
            }
        }

        let payload = fold.state().await?;
        Ok(Some(self.insert(target, &payload).await?.seq))
    }

    /// Persist a snapshot of `payload`, which must hold exactly the effect of
    /// the event log up to `seq`.
    pub async fn insert(&self, seq: u64, payload: &ProjectionSnapshot) -> Result<Snapshot> {
        let snapshot = Snapshot {
            id: uuid::Uuid::new_v4().to_string(),
            seq,
            created_at: Utc::now(),
            payload: payload.clone(),
            cut_version: SNAPSHOT_CUT_VERSION,
        };
        let payload_json =
            serde_json::to_string(payload).map_err(|e| CoreError::serde(e.to_string()))?;

        sqlx::query(
            "INSERT INTO snapshots (id, seq, created_at, payload_json, cut_version) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&snapshot.id)
        .bind(seq as i64)
        .bind(snapshot.created_at.to_rfc3339())
        .bind(payload_json)
        .bind(SNAPSHOT_CUT_VERSION as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(snapshot)
    }

    /// Drop all but the `keep` newest consistent snapshots, along with any
    /// row that can no longer be served. Returns rows removed.
    pub async fn prune_keep_latest(&self, keep: u32) -> Result<u64> {
        let removed = sqlx::query(
            "DELETE FROM snapshots WHERE id NOT IN \
             (SELECT id FROM snapshots WHERE cut_version = ? ORDER BY seq DESC LIMIT ?)",
        )
        .bind(SNAPSHOT_CUT_VERSION as i64)
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

    let cut_version: i64 = row
        .try_get("cut_version")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    let payload: ProjectionSnapshot =
        serde_json::from_str(&payload_json).map_err(|e| CoreError::serde(e.to_string()))?;

    Ok(Snapshot {
        id,
        seq: seq as u64,
        created_at: parse_ts(&created_at_s)?,
        payload,
        cut_version: cut_version as u32,
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_domain::{Actor, NewTask, TaskPatch};
    use daruma_events::Event;
    use daruma_shared::TaskId;

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

    /// A row as the pre-0063 writer left it.
    async fn insert_legacy(db: &Db, seq: i64) {
        sqlx::query(
            "INSERT INTO snapshots (id, seq, created_at, payload_json) \
             VALUES (?, ?, ?, '{\"tasks\":[],\"projects\":[],\"comments\":[]}')",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(seq)
        .bind(Utc::now().to_rfc3339())
        .execute(db.pool())
        .await
        .unwrap();
    }

    async fn append_task(store: &crate::SqliteEventStore, title: &str) -> EventEnvelope {
        let mut task = NewTask::new(title);
        task.id = Some(daruma_shared::TaskId::new());
        store
            .append(EventEnvelope::new(
                daruma_domain::Actor::user(),
                daruma_events::Event::TaskCreated { task },
            ))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn legacy_rows_are_never_served_and_pruned_first() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = SnapshotRepo::new(db.pool().clone());

        insert_legacy(&db, 50).await;
        assert!(repo.latest().await.unwrap().is_none());

        let sound = repo.insert(40, &payload_with_task("sound")).await.unwrap();
        insert_legacy(&db, 60).await;
        let latest = repo.latest().await.unwrap().unwrap();
        assert_eq!(latest.id, sound.id, "a higher legacy seq must not win");
        assert!(latest.is_consistent());

        assert_eq!(repo.prune_keep_latest(3).await.unwrap(), 2);
        assert_eq!(repo.latest().await.unwrap().unwrap().id, sound.id);
    }

    #[tokio::test]
    async fn fold_ignores_the_live_projections_and_builds_on_the_last_snapshot() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = SnapshotRepo::new(db.pool().clone());
        let store = crate::SqliteEventStore::new(db.pool().clone());

        assert_eq!(repo.write_if_due(&store, 2).await.unwrap(), None);
        append_task(&store, "first").await;
        assert_eq!(repo.write_if_due(&store, 2).await.unwrap(), None);
        let second = append_task(&store, "second").await;

        // Nothing was projected here: all of this comes from the log.
        assert_eq!(
            repo.write_if_due(&store, 2).await.unwrap(),
            Some(second.seq)
        );
        let snapshot = repo.latest().await.unwrap().unwrap();
        assert_eq!(snapshot.payload.tasks.len(), 2);

        let third = append_task(&store, "third").await;
        // The folded prefix is no longer in the log: only the base has it.
        sqlx::query("DELETE FROM events WHERE seq <= ?")
            .bind(second.seq as i64)
            .execute(db.pool())
            .await
            .unwrap();
        assert_eq!(repo.write_if_due(&store, 1).await.unwrap(), Some(third.seq));
        let snapshot = repo.latest().await.unwrap().unwrap();
        assert_eq!(snapshot.payload.tasks.len(), 3);
    }

    #[tokio::test]
    async fn fold_spans_more_than_one_page_and_stops_at_a_purged_tail() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = SnapshotRepo::new(db.pool().clone());
        let store = crate::SqliteEventStore::new(db.pool().clone());

        let mut last = 0;
        for i in 0..FOLD_PAGE_SIZE + 2 {
            last = append_task(&store, &format!("t{i}")).await.seq;
        }
        sqlx::query("DELETE FROM events WHERE seq = ?")
            .bind(last as i64)
            .execute(db.pool())
            .await
            .unwrap();

        assert_eq!(repo.write_if_due(&store, 1).await.unwrap(), Some(last));
        let snapshot = repo.latest().await.unwrap().unwrap();
        assert_eq!(snapshot.payload.tasks.len(), FOLD_PAGE_SIZE + 1);
    }

    #[tokio::test]
    async fn fold_hands_over_task_history_and_leaves_other_entities_alone() {
        let task_id = TaskId::new();
        let task = NewTask {
            id: Some(task_id),
            ..NewTask::new("t")
        };
        let fold = ProjectionFold::new(None).await.unwrap();
        for payload in [
            Event::TaskCreated { task },
            Event::TaskUpdated {
                task_id,
                patch: TaskPatch {
                    title: Some("renamed".into()),
                    ..TaskPatch::default()
                },
            },
        ] {
            let env = EventEnvelope::new(Actor::user(), payload);
            fold.apply(&env).await.unwrap();
        }
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let live = db.pool();
        sqlx::query(
            "INSERT INTO entity_versions (id, entity_type, entity_id, version_number, \
             actor_json, actor_kind, event_type, created_at, diff_json, summary) \
             VALUES (?, ?, ?, 1, '{}', 'user', 'x', '2026-01-01T00:00:00Z', '{}', 's')",
        )
        .bind("ver_stale")
        .bind("task")
        .bind(task_id.to_string())
        .execute(live)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO entity_versions (id, entity_type, entity_id, version_number, \
             actor_json, actor_kind, event_type, created_at, diff_json, summary) \
             VALUES (?, ?, ?, 1, '{}', 'user', 'x', '2026-01-01T00:00:00Z', '{}', 's')",
        )
        .bind("ver_doc")
        .bind("document")
        .bind("doc_1")
        .execute(live)
        .await
        .unwrap();

        let mut tx = live.begin().await.unwrap();
        fold.replace_task_versions_tx(&mut tx).await.unwrap();
        tx.commit().await.unwrap();

        let live_rows: Vec<(String, i64, String, Option<String>)> = sqlx::query_as(
            "SELECT entity_id, version_number, event_type, after_json FROM entity_versions \
             ORDER BY entity_type, version_number",
        )
        .fetch_all(live)
        .await
        .unwrap();
        let folded_rows: Vec<(String, i64, String, Option<String>)> = sqlx::query_as(
            "SELECT entity_id, version_number, event_type, after_json FROM entity_versions \
             ORDER BY entity_type, version_number",
        )
        .fetch_all(&fold.tasks.pool)
        .await
        .unwrap();
        assert_eq!(live_rows[0].0, "doc_1", "other entities stay");
        assert_eq!(live_rows[1..], folded_rows[..]);
        assert_eq!(folded_rows.len(), 2);
        assert!(folded_rows[1].3.as_deref().unwrap().contains("renamed"));
    }
}
