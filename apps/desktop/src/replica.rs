//! Local SQLite replica catch-up from the server event log.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use daruma_core::embed::{
    ActivityRepo, CommentRepo, EventEnvelope, EventStore, ProjectRepo, ProjectionFold,
    ProjectionSnapshot, Snapshot, SqliteEventStore, TaskRepo,
};
use daruma_shared::{CoreError, EventId, Result};
use sqlx::{Sqlite, SqliteExecutor, SqlitePool, Transaction};

/// Local event-log rows read per round during a rebuild.
const LOCAL_LOG_PAGE_SIZE: usize = 1000;

/// SQL for whether the rows can be trusted to match the cursor.
const VERIFIED: &str = "baseline_verified AND projections_verified";

/// The server side of a catch-up.
#[async_trait]
pub trait ReplicaSource: Send + Sync {
    async fn fetch_events(&self, since: u64, limit: u32) -> Result<Vec<EventEnvelope>>;
    async fn fetch_snapshot(&self) -> Result<Option<Snapshot>>;
}

pub struct Replica {
    pool: SqlitePool,
    store: Arc<dyn EventStore>,
    tasks: Arc<TaskRepo>,
    projects: Arc<ProjectRepo>,
    comments: Arc<CommentRepo>,
    activity: Arc<ActivityRepo>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatchUpStats {
    pub fetched: usize,
    pub applied: usize,
    pub server_seq: u64,
}

impl Replica {
    pub fn new(
        pool: SqlitePool,
        store: Arc<dyn EventStore>,
        tasks: Arc<TaskRepo>,
        projects: Arc<ProjectRepo>,
        comments: Arc<CommentRepo>,
        activity: Arc<ActivityRepo>,
    ) -> Self {
        Self {
            pool,
            store,
            tasks,
            projects,
            comments,
            activity,
        }
    }

    pub async fn ensure_schema(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS desktop_replica_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                server_seq INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        sqlx::query("INSERT OR IGNORE INTO desktop_replica_state (id, server_seq) VALUES (1, 0)")
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        // baseline_verified: 1 once the cursor is known to rest on a replay
        // from seq 0 or on a consistent snapshot. A cursor written before this
        // column existed may rest on a snapshot that missed events.
        // projections_verified: 1 once every event in the local log is known
        // to be projected. A sync that stopped between the append and the
        // projectors used to leave its event behind for good.
        // The column defaults send both kinds of replica through
        // `rebuild_from_log`.
        for column in ["baseline_verified", "projections_verified"] {
            let (present,) = sqlx::query_as::<_, (i64,)>(
                "SELECT COUNT(*) FROM pragma_table_info('desktop_replica_state') \
                 WHERE name = ?",
            )
            .bind(column)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
            if present == 0 {
                sqlx::query(&format!(
                    "ALTER TABLE desktop_replica_state \
                     ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0"
                ))
                .execute(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
            }
        }
        Ok(())
    }

    pub async fn server_seq(&self) -> Result<u64> {
        let row = sqlx::query_as::<_, (i64,)>(
            "SELECT server_seq FROM desktop_replica_state WHERE id = 1",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(row.0.max(0) as u64)
    }

    async fn baseline_verified(&self) -> Result<bool> {
        let row = sqlx::query_as::<_, (i64,)>(&format!(
            "SELECT {VERIFIED} FROM desktop_replica_state WHERE id = 1"
        ))
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(row.0 != 0)
    }

    /// Move the cursor onto a baseline known to hold every event up to it.
    async fn set_verified_baseline(&self, server_seq: u64) -> Result<()> {
        mark_verified_baseline(&self.pool, server_seq).await
    }

    pub async fn catch_up(&self, remote: &dyn ReplicaSource, limit: u32) -> Result<CatchUpStats> {
        // A brand new replica has nothing to repair; one that holds events
        // does even at cursor 0, since a first sync that stopped midway left
        // them in the log without writing a cursor.
        let repaired = if !self.baseline_verified().await?
            && (self.server_seq().await? > 0 || self.store.latest_seq().await? > 0)
        {
            self.rebuild_from_log(remote, limit).await?
        } else {
            0
        };
        let mut since = self.server_seq().await?;
        if since == 0 {
            // First catch-up on this device: try to bootstrap from the
            // server's materialised projection snapshot instead of replaying
            // the whole log from seq 0. No snapshot, an older server without
            // the endpoint, or a snapshot that cannot vouch for its seq → old
            // full-replay path unchanged.
            let snapshot = remote
                .fetch_snapshot()
                .await?
                .filter(Snapshot::is_consistent);
            if let Some(snapshot) = snapshot {
                tracing::info!(
                    seq = snapshot.seq,
                    tasks = snapshot.payload.tasks.len(),
                    projects = snapshot.payload.projects.len(),
                    comments = snapshot.payload.comments.len(),
                    "bootstrapping replica from server snapshot"
                );
                self.bootstrap_from_snapshot(&snapshot).await?;
                since = snapshot.seq;
            } else {
                self.set_verified_baseline(0).await?;
            }
        }
        let events = remote.fetch_events(since, limit).await?;
        let mut stats = self.apply_remote_events(events).await?;
        stats.fetched += repaired;
        Ok(stats)
    }

    /// Bootstrap from a server snapshot: restore the projection state and
    /// move the server cursor to `snapshot.seq`, so the next fetch replays
    /// only the delta. Restore goes through the same upsert SQL the
    /// projectors use.
    pub async fn bootstrap_from_snapshot(&self, snapshot: &Snapshot) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        self.restore_snapshot(&mut tx, &snapshot.payload).await?;
        mark_verified_baseline(&mut *tx, snapshot.seq).await?;
        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))
    }

    /// One-time repair for a replica whose rows may not match its cursor: the
    /// cursor rests on a snapshot which missed events, or on events that
    /// reached the local log but not the projectors. Those events sit at or
    /// below the cursor, so no delta can bring them back. Rebuild the
    /// projections as the server log up to the cursor plus whatever else this
    /// device has (events still in the outbox or refused by the server, and
    /// events past the cursor, which the next delta then finds done). Returns
    /// how many server events were read.
    ///
    /// Everything but the activity feed is folded on the side, so a failed
    /// fetch leaves the replica untouched. The feed comes from the local log:
    /// it only ever covered what this device saw. Other processes of this
    /// device keep writing meanwhile: the rows and the baseline change in one
    /// write transaction, which first reads whatever reached the local log
    /// since. A command that committed before it is in the fold, and any later
    /// one lands on the new rows. A crash rolls back, and the next sync reruns
    /// the repair. `status_changed_at` and project slug aliases are not
    /// rebuilt: only the server's audit and lookups read them.
    async fn rebuild_from_log(&self, remote: &dyn ReplicaSource, limit: u32) -> Result<usize> {
        let cursor = self.server_seq().await?;
        let fold = ProjectionFold::new(None).await?;
        let mut server_ids = HashSet::new();
        let mut since = 0;
        while since < cursor {
            let page = remote.fetch_events(since, limit).await?;
            let Some(last) = page.last() else { break };
            since = last.seq;
            for event in page.iter().filter(|e| e.seq <= cursor) {
                fold.apply(event).await?;
                server_ids.insert(event.id);
            }
        }
        if cursor > 0 && server_ids.is_empty() {
            // An empty log cannot be the one this cursor came from, and
            // adopting it would wipe the replica.
            return Err(CoreError::sync(format!(
                "server log is empty but the replica cursor is at {cursor}"
            )));
        }
        tracing::info!(
            cursor,
            events = server_ids.len(),
            "rebuilding replica from the server log"
        );
        // The bulk of the local log goes in before the lock is taken, so
        // writers only wait for the tail, the row swap and the history.
        let mut local_seq = 0;
        loop {
            let page = self
                .store
                .load_since(local_seq, LOCAL_LOG_PAGE_SIZE)
                .await?;
            if !fold_local_only(&fold, &server_ids, &mut local_seq, &page).await? {
                break;
            }
        }

        let mut tx = self.begin_write().await?;
        let (current, verified) = sqlx::query_as::<_, (i64, i64)>(&format!(
            "SELECT server_seq, {VERIFIED} FROM desktop_replica_state WHERE id = 1"
        ))
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        if verified != 0 {
            // Another sync of this device finished the repair.
            return Ok(server_ids.len());
        }
        loop {
            let page =
                SqliteEventStore::load_since_tx(&mut tx, local_seq, LOCAL_LOG_PAGE_SIZE).await?;
            if !fold_local_only(&fold, &server_ids, &mut local_seq, &page).await? {
                break;
            }
        }
        let state = fold.state().await?;
        self.replace_projections(&mut tx, &state).await?;
        fold.replace_task_versions_tx(&mut tx).await?;
        self.replace_activity(&mut tx).await?;
        mark_verified_baseline(&mut *tx, (current.max(0) as u64).max(cursor)).await?;
        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(server_ids.len())
    }

    /// Replay the activity feed from the local log. A row comes from exactly
    /// one logged event, and pair merges look at the neighbouring seq, so
    /// starting over gives what an uninterrupted replay gives.
    async fn replace_activity(&self, tx: &mut Transaction<'_, Sqlite>) -> Result<()> {
        sqlx::query("DELETE FROM activity")
            .execute(&mut **tx)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let mut local_seq = 0;
        loop {
            let page = SqliteEventStore::load_since_tx(tx, local_seq, LOCAL_LOG_PAGE_SIZE).await?;
            let Some(last) = page.last() else {
                return Ok(());
            };
            local_seq = last.seq;
            for event in &page {
                self.activity.apply_event_tx(tx, event).await?;
            }
        }
    }

    /// Make the projections hold exactly `state`. Only rows `state` lacks are
    /// deleted: task side tables cascade from `tasks`, and rows that stay
    /// must keep them.
    async fn replace_projections(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        state: &ProjectionSnapshot,
    ) -> Result<()> {
        sqlx::query("DELETE FROM comments")
            .execute(&mut **tx)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let kept: HashSet<_> = state.tasks.iter().map(|t| t.id.to_string()).collect();
        for id in row_ids(tx, "SELECT id FROM tasks").await? {
            if !kept.contains(&id) {
                for sql in [
                    "DELETE FROM tasks WHERE id = ?",
                    "DELETE FROM task_due_notifications WHERE task_id = ?",
                ] {
                    sqlx::query(sql)
                        .bind(&id)
                        .execute(&mut **tx)
                        .await
                        .map_err(|e| CoreError::storage(e.to_string()))?;
                }
            }
        }
        let kept: HashSet<_> = state.projects.iter().map(|p| p.id.to_string()).collect();
        for id in row_ids(tx, "SELECT id FROM projects").await? {
            if !kept.contains(&id) {
                sqlx::query("DELETE FROM projects WHERE id = ?")
                    .bind(&id)
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }
        }
        self.restore_snapshot(tx, state).await
    }

    async fn restore_snapshot(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        payload: &ProjectionSnapshot,
    ) -> Result<()> {
        // Restore parents before children (projects → tasks → comments) to
        // match the replay order a full catch-up would produce.
        for project in &payload.projects {
            self.projects.upsert_project_tx(tx, project).await?;
        }
        for task in &payload.tasks {
            self.tasks.upsert_task_tx(tx, task).await?;
        }
        for comment in &payload.comments {
            self.comments.upsert_comment_tx(tx, comment).await?;
        }
        Ok(())
    }

    pub async fn apply_remote_events(&self, events: Vec<EventEnvelope>) -> Result<CatchUpStats> {
        let mut applied = 0usize;
        let mut max_server_seq = self.server_seq().await?;
        let fetched = events.len();

        // The first failure ends the batch: the cursor stays on the last
        // event that took effect, so no later one can hide the gap.
        for event in events {
            max_server_seq = max_server_seq.max(event.seq);
            if self.apply_remote_event(event).await? {
                applied += 1;
            }
        }

        Ok(CatchUpStats {
            fetched,
            applied,
            server_seq: max_server_seq,
        })
    }

    /// Log one server event, run every projector and move the cursor past it,
    /// all in one transaction. An event in the local log has therefore always
    /// taken full effect, so finding its id means there is nothing left to do,
    /// whether it is a retry or the echo of a local command. `false` for those.
    async fn apply_remote_event(&self, mut event: EventEnvelope) -> Result<bool> {
        let server_seq = event.seq;
        let mut tx = self.begin_write().await?;
        let unseen = SqliteEventStore::load_by_id_tx(&mut tx, event.id)
            .await?
            .is_none();
        if unseen {
            event.seq = 0;
            let persisted = SqliteEventStore::append_tx(&mut tx, event).await?;
            self.tasks.apply_event_tx(&mut tx, &persisted).await?;
            self.projects.apply_event_tx(&mut tx, &persisted).await?;
            self.comments.apply_event_tx(&mut tx, &persisted).await?;
            self.activity.apply_event_tx(&mut tx, &persisted).await?;
        }
        advance_server_seq(&mut *tx, server_seq).await?;
        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(unseen)
    }

    /// A transaction that holds the write lock from its first statement, so
    /// what it reads cannot change before it writes.
    async fn begin_write(&self) -> Result<Transaction<'_, Sqlite>> {
        self.pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| CoreError::storage(e.to_string()))
    }
}

/// Two processes of one device may sync at once, so the cursor only moves up.
async fn advance_server_seq<'e, E: SqliteExecutor<'e>>(executor: E, server_seq: u64) -> Result<()> {
    sqlx::query("UPDATE desktop_replica_state SET server_seq = MAX(server_seq, ?) WHERE id = 1")
        .bind(server_seq as i64)
        .execute(executor)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
    Ok(())
}

async fn mark_verified_baseline<'e, E: SqliteExecutor<'e>>(
    executor: E,
    server_seq: u64,
) -> Result<()> {
    sqlx::query(
        "UPDATE desktop_replica_state \
         SET server_seq = ?, baseline_verified = 1, projections_verified = 1 WHERE id = 1",
    )
    .bind(server_seq as i64)
    .execute(executor)
    .await
    .map_err(|e| CoreError::storage(e.to_string()))?;
    Ok(())
}

/// Fold one page of the local log, skipping what the server already has, and
/// move `local_seq` past it. `false` once the log is exhausted.
async fn fold_local_only(
    fold: &ProjectionFold,
    server_ids: &HashSet<EventId>,
    local_seq: &mut u64,
    page: &[EventEnvelope],
) -> Result<bool> {
    let Some(last) = page.last() else {
        return Ok(false);
    };
    *local_seq = last.seq;
    for event in page.iter().filter(|e| !server_ids.contains(&e.id)) {
        fold.apply(event).await?;
    }
    Ok(true)
}

async fn row_ids(tx: &mut Transaction<'_, Sqlite>, sql: &str) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(sql)
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_core::{
        embed::{Command, CommandBus, Db, Event, EventBus, SnapshotRepo},
        CommandHandler,
    };
    use daruma_domain::{
        Actor, Comment, CommentPatch, NewComment, NewTask, Project, Status, TaskPatch,
    };
    use daruma_shared::TaskId;
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::{Mutex, Notify};

    use crate::{local_executor::LocalExecutor, outbox::Outbox};

    struct Device {
        replica: Replica,
        tasks: Arc<TaskRepo>,
        projects: Arc<ProjectRepo>,
        comments: Arc<CommentRepo>,
        pool: SqlitePool,
        store: Arc<dyn EventStore>,
    }

    impl Device {
        /// The replicated projections, comparable across replicas.
        /// `updated_event_seq` tracks the replica-local log position; a
        /// snapshot-bootstrapped replica has no local rows for pre-snapshot
        /// events, so it is normalised away. Rows are sorted by id: replays
        /// give tasks of one command the same `created_at`.
        async fn state(&self) -> ProjectionSnapshot {
            let mut tasks = self.tasks.list_all().await.unwrap();
            for t in &mut tasks {
                t.updated_event_seq = None;
            }
            tasks.sort_by_key(|t| t.id.to_string());
            let mut projects = self.projects.list_all().await.unwrap();
            projects.sort_by_key(|p| p.id.to_string());
            let mut comments = self.comments.list_all().await.unwrap();
            comments.sort_by_key(|c| c.id.to_string());
            ProjectionSnapshot {
                tasks,
                projects,
                comments,
            }
        }

        /// A local command: logged, applied and held back from the server.
        async fn local_event(&self, payload: Event) -> EventEnvelope {
            let env = self
                .store
                .append(EventEnvelope::new(Actor::user(), payload))
                .await
                .unwrap();
            self.tasks.apply_event(&env).await.unwrap();
            self.projects.apply_event(&env).await.unwrap();
            self.comments.apply_event(&env).await.unwrap();
            env
        }

        /// What a replica looks like after bootstrapping from a pre-0063
        /// snapshot: restored state, cursor, no proof the two agree.
        async fn bootstrapped_from_legacy(&self, payload: &ProjectionSnapshot, seq: u64) {
            let mut tx = self.pool.begin().await.unwrap();
            self.replica
                .restore_snapshot(&mut tx, payload)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            advance_server_seq(&self.pool, seq).await.unwrap();
        }
    }

    async fn fresh_device() -> Device {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let pool = db.pool().clone();
        device_on(pool.clone(), Arc::new(SqliteEventStore::new(pool))).await
    }

    async fn device_on(pool: SqlitePool, store: Arc<dyn EventStore>) -> Device {
        let tasks = Arc::new(TaskRepo::new(pool.clone()));
        let projects = Arc::new(ProjectRepo::new(pool.clone()));
        let comments = Arc::new(CommentRepo::new(pool.clone()));
        let activity = Arc::new(ActivityRepo::new(pool.clone()));
        let replica = Replica::new(
            pool.clone(),
            store.clone(),
            tasks.clone(),
            projects.clone(),
            comments.clone(),
            activity,
        );
        replica.ensure_schema().await.unwrap();
        Device {
            replica,
            tasks,
            projects,
            comments,
            pool,
            store,
        }
    }

    #[tokio::test]
    async fn applies_remote_events_once_and_advances_server_cursor() {
        let device = fresh_device().await;

        let mut event = EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated {
                task: NewTask::new("remote"),
            },
        );
        event.seq = 7;

        let stats = device
            .replica
            .apply_remote_events(vec![event.clone()])
            .await
            .unwrap();
        assert_eq!(stats.applied, 1);
        assert_eq!(stats.server_seq, 7);
        assert_eq!(device.tasks.list_all().await.unwrap().len(), 1);

        let stats = device
            .replica
            .apply_remote_events(vec![event])
            .await
            .unwrap();
        assert_eq!(stats.applied, 0);
        assert_eq!(stats.server_seq, 7);
        assert_eq!(device.tasks.list_all().await.unwrap().len(), 1);
    }

    /// Append an event to the "server" log and run the same write-through
    /// projectors the server runs (`apply_persisted_event`).
    async fn seed_server_event(
        store: &SqliteEventStore,
        tasks: &TaskRepo,
        projects: &ProjectRepo,
        comments: &CommentRepo,
        payload: Event,
    ) -> EventEnvelope {
        let env = store
            .append(EventEnvelope::new(Actor::user(), payload))
            .await
            .unwrap();
        tasks.apply_event(&env).await.unwrap();
        projects.apply_event(&env).await.unwrap();
        comments.apply_event(&env).await.unwrap();
        env
    }

    /// Snapshot bootstrapping must land the replica in the same projection
    /// state as a full replay from seq 0 (the no-snapshot fallback), while
    /// fetching only the delta.
    #[tokio::test]
    async fn snapshot_bootstrap_matches_full_replay() {
        // ── "server": event log + write-through projections ──────────────────
        let server_db = Db::memory().await.unwrap();
        server_db.migrate().await.unwrap();
        let spool = server_db.pool().clone();
        let s_store = SqliteEventStore::new(spool.clone());
        let s_tasks = TaskRepo::new(spool.clone());
        let s_projects = ProjectRepo::new(spool.clone());
        let s_comments = CommentRepo::new(spool.clone());
        let s_snapshots = SnapshotRepo::new(spool.clone());

        // Pre-snapshot history. Task ids are assigned explicitly here, exactly
        // as `CommandHandler` does before emitting `TaskCreated` — a `NewTask`
        // with `id: None` would be materialised with a fresh random id by each
        // projector run, making replay/snapshot non-deterministic.
        let project = Project::new("demo", None);
        let project_id = project.id;
        let mut new_t1 = NewTask::new("t1");
        new_t1.id = Some(TaskId::new());
        new_t1.project_id = Some(project_id);
        let t1 = new_t1.id.unwrap();
        let mut new_t2 = NewTask::new("t2");
        new_t2.id = Some(TaskId::new());
        let t2 = new_t2.id.unwrap();
        let comment = Comment::from_new(
            NewComment {
                id: None,
                task_id: t1,
                body: "v1".into(),
                parent_id: None,
                kind: None,
            },
            Actor::user(),
            chrono::Utc::now(),
        );
        let comment_id = comment.id;

        for payload in [
            Event::ProjectCreated { project },
            Event::TaskCreated { task: new_t1 },
            Event::TaskCreated { task: new_t2 },
            Event::TaskUpdated {
                task_id: t1,
                patch: TaskPatch {
                    title: Some("t1 renamed".into()),
                    ..TaskPatch::default()
                },
            },
            Event::TaskStatusChanged {
                task_id: t2,
                from: Status::Inbox,
                to: Status::InProgress,
            },
            Event::CommentAdded { comment },
            Event::CommentEdited {
                comment_id,
                task_id: t1,
                patch: CommentPatch {
                    body: Some("v2".into()),
                },
                edited_at: chrono::Utc::now(),
            },
        ] {
            seed_server_event(&s_store, &s_tasks, &s_projects, &s_comments, payload).await;
        }

        // Take the snapshot exactly as the background writer would.
        let snapshot_seq = s_snapshots
            .write_if_due(&s_store, 1)
            .await
            .unwrap()
            .expect("snapshot is due");
        let snapshot = s_snapshots.latest().await.unwrap().expect("just written");

        // Post-snapshot delta.
        let mut new_t3 = NewTask::new("t3");
        new_t3.id = Some(TaskId::new());
        for payload in [
            Event::TaskCreated { task: new_t3 },
            Event::TaskUpdated {
                task_id: t1,
                patch: TaskPatch {
                    description: Some("delta description".into()),
                    ..TaskPatch::default()
                },
            },
            Event::CommentAdded {
                comment: Comment::from_new(
                    NewComment {
                        id: None,
                        task_id: t2,
                        body: "delta comment".into(),
                        parent_id: None,
                        kind: None,
                    },
                    Actor::user(),
                    chrono::Utc::now(),
                ),
            },
        ] {
            seed_server_event(&s_store, &s_tasks, &s_projects, &s_comments, payload).await;
        }

        // ── device A: no snapshot available → full replay (fallback) ─────────
        let a = fresh_device().await;
        let all = s_store.load_since(0, 1000).await.unwrap();
        let stats_a = a.replica.apply_remote_events(all).await.unwrap();

        // ── device B: bootstrap from snapshot, then replay only the delta ────
        let b = fresh_device().await;
        b.replica.bootstrap_from_snapshot(&snapshot).await.unwrap();
        assert_eq!(b.replica.server_seq().await.unwrap(), snapshot_seq);
        let delta = s_store.load_since(snapshot_seq, 1000).await.unwrap();
        assert_eq!(delta.len(), 3, "only post-snapshot events are fetched");
        let stats_b = b.replica.apply_remote_events(delta).await.unwrap();

        // ── equivalence checkpoints ──────────────────────────────────────────
        assert_eq!(stats_a.server_seq, stats_b.server_seq);
        assert_eq!(stats_a.fetched, 10);
        assert_eq!(stats_b.fetched, 3);

        assert_eq!(a.state().await, b.state().await);

        // Spot-check the actual content, not just pairwise equality.
        let b_tasks = b.tasks.list_all().await.unwrap();
        assert_eq!(b_tasks.len(), 3);
        let t1_restored = b_tasks.iter().find(|t| t.id == t1).unwrap();
        assert_eq!(t1_restored.title, "t1 renamed");
        assert_eq!(t1_restored.description, "delta description");
        assert_eq!(t1_restored.project_id, Some(project_id));
        let b_c1 = b.comments.get(comment_id).await.unwrap().unwrap();
        assert_eq!(b_c1.body, "v2");
        assert!(b_c1.edited_at.is_some());
    }

    // ── snapshot consistency: event log vs projections ───────────────────────

    /// Forwards to the real log and can stop a command right after
    /// `append_batch` has committed, before any projector ran.
    struct PausingStore {
        inner: SqliteEventStore,
        armed: AtomicBool,
        committed: Notify,
        resume: Notify,
    }

    #[async_trait]
    impl EventStore for PausingStore {
        async fn append(&self, envelope: EventEnvelope) -> Result<EventEnvelope> {
            self.inner.append(envelope).await
        }

        async fn append_batch(&self, envelopes: Vec<EventEnvelope>) -> Result<Vec<EventEnvelope>> {
            let persisted = self.inner.append_batch(envelopes).await?;
            if self.armed.swap(false, Ordering::SeqCst) {
                self.committed.notify_one();
                self.resume.notified().await;
            }
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

    /// A server wired as in production: commands go through `CommandHandler`,
    /// projections are written through, `/v1/events*` read the log.
    struct Server {
        pool: SqlitePool,
        store: Arc<PausingStore>,
        tasks: Arc<TaskRepo>,
        snapshots: SnapshotRepo,
        commands: CommandBus,
        /// Serves whatever `latest` would have, or a pre-0063 row verbatim.
        served_snapshot: Mutex<Option<Snapshot>>,
    }

    impl Server {
        async fn start() -> Self {
            let db = Db::memory().await.unwrap();
            db.migrate().await.unwrap();
            let pool = db.pool().clone();
            let store = Arc::new(PausingStore {
                inner: SqliteEventStore::new(pool.clone()),
                armed: AtomicBool::new(false),
                committed: Notify::new(),
                resume: Notify::new(),
            });
            let tasks = Arc::new(TaskRepo::new(pool.clone()));
            let commands = CommandBus::new(Arc::new(CommandHandler::new(
                store.clone(),
                tasks.clone(),
                Arc::new(ProjectRepo::new(pool.clone())),
                Arc::new(CommentRepo::new(pool.clone())),
                Arc::new(ActivityRepo::new(pool.clone())),
                EventBus::new(16),
            )));
            Self {
                snapshots: SnapshotRepo::new(pool.clone()),
                pool,
                store,
                tasks,
                commands,
                served_snapshot: Mutex::new(None),
            }
        }

        async fn create_task(&self, title: &str) -> TaskId {
            let mut task = NewTask::new(title);
            let id = TaskId::new();
            task.id = Some(id);
            self.commands
                .dispatch(Command::CreateTask { task }, Actor::user())
                .await
                .unwrap();
            id
        }

        async fn write_snapshot(&self) -> u64 {
            let seq = self
                .snapshots
                .write_if_due(&*self.store, 1)
                .await
                .unwrap()
                .expect("snapshot is due");
            *self.served_snapshot.lock().await = self.snapshots.latest().await.unwrap();
            seq
        }

        /// A device that replayed the whole log, the reference state.
        async fn full_replay(&self) -> Device {
            let device = fresh_device().await;
            let all = self.store.load_since(0, usize::MAX >> 1).await.unwrap();
            device.replica.apply_remote_events(all).await.unwrap();
            device
        }

        /// Run `cmd` up to the commit of its events and leave it hanging
        /// there, event committed and projections untouched.
        async fn dispatch_until_committed(&self, cmd: Command) -> tokio::task::JoinHandle<()> {
            self.store.armed.store(true, Ordering::SeqCst);
            let commands = self.commands.clone();
            let handle = tokio::spawn(async move {
                commands.dispatch(cmd, Actor::user()).await.unwrap();
            });
            self.store.committed.notified().await;
            handle
        }

        async fn resume(&self, handle: tokio::task::JoinHandle<()>) {
            self.store.resume.notify_one();
            handle.await.unwrap();
        }
    }

    #[async_trait]
    impl ReplicaSource for Server {
        async fn fetch_events(&self, since: u64, limit: u32) -> Result<Vec<EventEnvelope>> {
            self.store.load_since(since, limit.min(1000) as usize).await
        }

        async fn fetch_snapshot(&self) -> Result<Option<Snapshot>> {
            Ok(self.served_snapshot.lock().await.clone())
        }
    }

    /// Catch up until the device has seen the whole log.
    async fn catch_up_fully(device: &Device, server: &Server) {
        let target = server.store.latest_seq().await.unwrap();
        for _ in 0..1000 {
            device.replica.catch_up(server, 2).await.unwrap();
            if device.replica.server_seq().await.unwrap() >= target {
                return;
            }
        }
        panic!("catch-up never reached seq {target}");
    }

    #[tokio::test]
    async fn snapshot_between_commit_and_apply_still_holds_the_event() {
        let server = Server::start().await;
        server.create_task("before").await;

        let mut task = NewTask::new("committed, not applied");
        let id = TaskId::new();
        task.id = Some(id);
        let command = server
            .dispatch_until_committed(Command::CreateTask { task })
            .await;
        assert!(
            server.tasks.get(id).await.unwrap().is_none(),
            "the live projection must still lack the event here"
        );

        let seq = server.write_snapshot().await;
        assert_eq!(seq, server.store.latest_seq().await.unwrap());
        server.resume(command).await;

        let device = fresh_device().await;
        catch_up_fully(&device, &server).await;

        assert!(device.tasks.get(id).await.unwrap().is_some());
        assert_eq!(
            device.state().await,
            server.full_replay().await.state().await
        );
    }

    #[tokio::test]
    async fn snapshot_between_commit_and_apply_of_a_multi_event_command() {
        let server = Server::start().await;
        let parent = server.create_task("parent").await;

        let command = server
            .dispatch_until_committed(Command::SplitTask {
                parent,
                subtasks: vec![NewTask::new("sub a"), NewTask::new("sub b")],
            })
            .await;
        server.write_snapshot().await;
        server.resume(command).await;

        let device = fresh_device().await;
        catch_up_fully(&device, &server).await;

        let state = device.state().await;
        assert!(state.tasks.iter().any(|t| t.title == "sub a"));
        assert!(state.tasks.iter().any(|t| t.title == "sub b"));
        assert_eq!(state, server.full_replay().await.state().await);
    }

    #[tokio::test]
    async fn events_landing_during_the_fold_stay_in_the_delta() {
        /// Lands a write as the fold starts to read: after `target` is
        /// fixed, between what a live read would see of each projection.
        struct WriteDuringFold {
            server: Arc<Server>,
            done: AtomicBool,
        }

        #[async_trait]
        impl EventStore for WriteDuringFold {
            async fn append(&self, envelope: EventEnvelope) -> Result<EventEnvelope> {
                self.server.store.append(envelope).await
            }
            async fn append_batch(&self, e: Vec<EventEnvelope>) -> Result<Vec<EventEnvelope>> {
                self.server.store.append_batch(e).await
            }
            async fn load_since(&self, since: u64, limit: usize) -> Result<Vec<EventEnvelope>> {
                if !self.done.swap(true, Ordering::SeqCst) {
                    self.server.create_task("during the fold").await;
                }
                self.server.store.load_since(since, limit).await
            }
            async fn load_by_id(&self, id: EventId) -> Result<Option<EventEnvelope>> {
                self.server.store.load_by_id(id).await
            }
            async fn latest_seq(&self) -> Result<u64> {
                self.server.store.latest_seq().await
            }
        }

        let server = Arc::new(Server::start().await);
        server.create_task("before").await;
        let seq_before = server.store.latest_seq().await.unwrap();
        let racing = WriteDuringFold {
            server: server.clone(),
            done: AtomicBool::new(false),
        };

        let seq = server
            .snapshots
            .write_if_due(&racing, 1)
            .await
            .unwrap()
            .unwrap();
        *server.served_snapshot.lock().await = server.snapshots.latest().await.unwrap();

        assert_eq!(seq, seq_before, "seq is the cut the fold started from");
        let snapshot = server.snapshots.latest().await.unwrap().unwrap();
        assert_eq!(snapshot.payload.tasks.len(), 1, "nothing past the cut");

        let device = fresh_device().await;
        catch_up_fully(&device, &server).await;
        assert_eq!(device.state().await.tasks.len(), 2);
        assert_eq!(
            device.state().await,
            server.full_replay().await.state().await
        );
    }

    #[tokio::test]
    async fn failed_live_apply_does_not_hide_the_event_from_a_new_device() {
        let server = Server::start().await;
        server.create_task("before").await;
        sqlx::query(
            "CREATE TRIGGER poison BEFORE INSERT ON tasks WHEN NEW.title = 'poison' \
             BEGIN SELECT RAISE(ABORT, 'projection down'); END",
        )
        .execute(&server.pool)
        .await
        .unwrap();

        let mut task = NewTask::new("poison");
        task.id = Some(TaskId::new());
        let failed = server
            .commands
            .dispatch(Command::CreateTask { task }, Actor::user())
            .await;
        assert!(failed.is_err(), "the projector must have failed");
        assert_eq!(server.tasks.list_all().await.unwrap().len(), 1);
        server.create_task("after").await;

        server.write_snapshot().await;

        let device = fresh_device().await;
        catch_up_fully(&device, &server).await;
        assert_eq!(device.state().await.tasks.len(), 3);
        assert_eq!(
            device.state().await,
            server.full_replay().await.state().await
        );
    }

    #[tokio::test]
    async fn event_that_cannot_be_folded_keeps_the_served_snapshot() {
        let server = Server::start().await;
        server.create_task("before").await;
        let seq = server.write_snapshot().await;
        server.create_task("unreadable").await;
        sqlx::query("UPDATE events SET payload_json = '{}' WHERE seq > ?")
            .bind(seq as i64)
            .execute(&server.pool)
            .await
            .unwrap();

        assert!(server
            .snapshots
            .write_if_due(&*server.store, 1)
            .await
            .is_err());
        assert_eq!(server.snapshots.latest().await.unwrap().unwrap().seq, seq);
    }

    #[tokio::test]
    async fn device_ignores_a_snapshot_that_cannot_vouch_for_its_seq() {
        let server = Server::start().await;
        let kept = server.create_task("in the log").await;
        let seq = server.store.latest_seq().await.unwrap();
        // The shape an older server answers with: no `cut_version` at all.
        let legacy: Snapshot = serde_json::from_value(serde_json::json!({
            "id": "legacy",
            "seq": seq,
            "created_at": chrono::Utc::now(),
            "payload": ProjectionSnapshot::default(),
        }))
        .unwrap();
        *server.served_snapshot.lock().await = Some(legacy);

        let device = fresh_device().await;
        catch_up_fully(&device, &server).await;

        assert!(device.tasks.get(kept).await.unwrap().is_some());
        assert_eq!(device.replica.server_seq().await.unwrap(), seq);
    }

    #[tokio::test]
    async fn catch_up_without_any_snapshot_replays_from_zero() {
        let server = Server::start().await;
        server.create_task("a").await;
        server.create_task("b").await;
        server.create_task("c").await;

        let device = fresh_device().await;
        catch_up_fully(&device, &server).await;

        assert_eq!(
            device.state().await,
            server.full_replay().await.state().await
        );
    }

    /// A device stranded by the old writer: cursor at N, N's effect missing.
    async fn stranded_device(server: &Server) -> (Device, TaskId) {
        let device = fresh_device().await;
        server.create_task("seen").await;
        let seen = server.full_replay().await.state().await;
        let missing = server.create_task("missing from the snapshot").await;
        let cursor = server.store.latest_seq().await.unwrap();
        device.bootstrapped_from_legacy(&seen, cursor).await;
        (device, missing)
    }

    #[tokio::test]
    async fn stranded_device_recovers_the_event_below_its_cursor() {
        let server = Server::start().await;
        let (device, missing) = stranded_device(&server).await;

        let stats = device.replica.catch_up(&server, 1).await.unwrap();

        assert!(device.tasks.get(missing).await.unwrap().is_some());
        assert_eq!(stats.fetched, 2, "the repair pages through the whole log");
        assert_eq!(
            device.state().await,
            server.full_replay().await.state().await
        );

        // Once only: later rounds are ordinary deltas.
        let stats = device.replica.catch_up(&server, 1).await.unwrap();
        assert_eq!(stats.fetched, 0);
    }

    #[tokio::test]
    async fn repair_keeps_local_only_events_and_later_echoes_stay_applied() {
        let server = Server::start().await;
        let (device, missing) = stranded_device(&server).await;

        let mut offline = NewTask::new("created offline");
        let offline_id = TaskId::new();
        offline.id = Some(offline_id);
        let created = device
            .local_event(Event::TaskCreated { task: offline })
            .await;
        let edited = device
            .local_event(Event::TaskUpdated {
                task_id: missing,
                patch: TaskPatch {
                    title: Some("edited offline".into()),
                    ..TaskPatch::default()
                },
            })
            .await;

        device.replica.catch_up(&server, 50).await.unwrap();

        let task = device.tasks.get(offline_id).await.unwrap();
        assert_eq!(task.unwrap().title, "created offline");
        let task = device.tasks.get(missing).await.unwrap();
        assert_eq!(
            task.unwrap().title,
            "edited offline",
            "a pending edit lands on the recovered row"
        );

        // The pending events reach the server later and echo back.
        for event in [created, edited] {
            let mut pushed = event.clone();
            pushed.seq = 0;
            let env = server.store.append(pushed).await.unwrap();
            server.tasks.apply_event(&env).await.unwrap();
        }
        let stats = device.replica.catch_up(&server, 50).await.unwrap();
        assert_eq!((stats.fetched, stats.applied), (2, 0));
        assert_eq!(
            device.state().await,
            server.full_replay().await.state().await
        );
    }

    #[tokio::test]
    async fn repair_drops_rows_the_log_does_not_produce() {
        let server = Server::start().await;
        let (device, _missing) = stranded_device(&server).await;
        let mut ghost = daruma_domain::Task::from_new(NewTask::new("deleted on the server"));
        ghost.id = TaskId::new();
        device.tasks.upsert_task(&ghost).await.unwrap();

        device.replica.catch_up(&server, 50).await.unwrap();

        assert!(device.tasks.get(ghost.id).await.unwrap().is_none());
        assert_eq!(
            device.state().await,
            server.full_replay().await.state().await
        );
    }

    #[tokio::test]
    async fn failed_repair_leaves_the_replica_untouched_and_retries() {
        struct Offline;

        #[async_trait]
        impl ReplicaSource for Offline {
            async fn fetch_events(&self, _: u64, _: u32) -> Result<Vec<EventEnvelope>> {
                Err(CoreError::sync("connection refused"))
            }
            async fn fetch_snapshot(&self) -> Result<Option<Snapshot>> {
                Ok(None)
            }
        }

        let server = Server::start().await;
        let (device, missing) = stranded_device(&server).await;
        let before = device.state().await;

        assert!(device.replica.catch_up(&Offline, 50).await.is_err());
        assert_eq!(device.state().await, before);

        device.replica.catch_up(&server, 50).await.unwrap();
        assert!(device.tasks.get(missing).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn repair_interrupted_after_touching_rows_rolls_back_and_reruns() {
        let server = Server::start().await;
        let (device, missing) = stranded_device(&server).await;
        let mut ghost = daruma_domain::Task::from_new(NewTask::new("deleted on the server"));
        ghost.id = TaskId::new();
        device.tasks.upsert_task(&ghost).await.unwrap();
        let mut offline = NewTask::new("created offline");
        let offline_id = TaskId::new();
        offline.id = Some(offline_id);
        device
            .local_event(Event::TaskCreated { task: offline })
            .await;
        // Lets the rebuild replace the rows, then stops it on the local event.
        let trigger = "CREATE TRIGGER stop_rebuild BEFORE INSERT ON tasks \
             WHEN NEW.title = 'created offline' \
             BEGIN SELECT RAISE(ABORT, 'interrupted'); END";
        sqlx::query(trigger).execute(&device.pool).await.unwrap();

        let before = device.state().await;
        assert!(device.replica.catch_up(&server, 50).await.is_err());
        assert!(!device.replica.baseline_verified().await.unwrap());
        assert_eq!(device.state().await, before, "the swap is all or nothing");

        sqlx::query("DROP TRIGGER stop_rebuild")
            .execute(&device.pool)
            .await
            .unwrap();
        device.replica.catch_up(&server, 50).await.unwrap();

        assert!(device.replica.baseline_verified().await.unwrap());
        assert!(device.tasks.get(missing).await.unwrap().is_some());
        assert!(device.tasks.get(offline_id).await.unwrap().is_some());
        assert!(device.tasks.get(ghost.id).await.unwrap().is_none());
    }

    // ── repair next to other processes of the same device ────────────────────

    type Hook = Pin<Box<dyn Future<Output = ()> + Send>>;

    /// Runs a hook once the repair has read the last page of the local log.
    struct AfterLocalLogRead {
        inner: SqliteEventStore,
        hook: Mutex<Option<Hook>>,
    }

    #[async_trait]
    impl EventStore for AfterLocalLogRead {
        async fn append(&self, envelope: EventEnvelope) -> Result<EventEnvelope> {
            self.inner.append(envelope).await
        }

        async fn append_batch(&self, envelopes: Vec<EventEnvelope>) -> Result<Vec<EventEnvelope>> {
            self.inner.append_batch(envelopes).await
        }

        async fn load_since(&self, since_seq: u64, limit: usize) -> Result<Vec<EventEnvelope>> {
            let page = self.inner.load_since(since_seq, limit).await?;
            if page.is_empty() {
                let hook = self.hook.lock().await.take();
                if let Some(hook) = hook {
                    hook.await;
                }
            }
            Ok(page)
        }

        async fn load_by_id(&self, id: EventId) -> Result<Option<EventEnvelope>> {
            self.inner.load_by_id(id).await
        }

        async fn latest_seq(&self) -> Result<u64> {
            self.inner.latest_seq().await
        }
    }

    /// One `replica.sqlite`. Each `open` is a separate connection pool on it,
    /// as a separate `daruma-desktop` process would have.
    struct ReplicaFile {
        dir: PathBuf,
    }

    /// A process of the device: the catch-up side and real local commands.
    struct Process {
        device: Device,
        local: LocalExecutor,
    }

    impl ReplicaFile {
        async fn create() -> Self {
            let dir = std::env::temp_dir().join(format!("daruma-replica-{}", TaskId::new()));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        async fn open(
            &self,
            wrap: impl FnOnce(SqliteEventStore) -> Arc<dyn EventStore>,
        ) -> Process {
            let path = self.dir.join("replica.sqlite");
            let db = Db::open(path.to_str().unwrap()).await.unwrap();
            db.migrate().await.unwrap();
            let pool = db.pool().clone();
            let store = wrap(SqliteEventStore::new(pool.clone()));
            let device = device_on(pool, store.clone()).await;
            let commands = CommandBus::new(Arc::new(CommandHandler::new(
                store,
                device.tasks.clone(),
                device.projects.clone(),
                device.comments.clone(),
                device.replica.activity.clone(),
                EventBus::new(16),
            )));
            let outbox = Outbox::new(db);
            outbox.ensure_schema().await.unwrap();
            let local = LocalExecutor::new(commands, outbox).await.unwrap();
            Process { device, local }
        }

        async fn open_plain(&self) -> Process {
            self.open(|store| Arc::new(store)).await
        }

        /// Open a process whose repair calls `hook` after its last local read.
        async fn open_hooked(&self, hook: Hook) -> Process {
            self.open(|inner| {
                Arc::new(AfterLocalLogRead {
                    inner,
                    hook: Mutex::new(Some(hook)),
                })
            })
            .await
        }
    }

    impl Drop for ReplicaFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Process {
        /// Put the replica where an old bootstrap left it: the server's
        /// "seen" task, a cursor past the event it missed.
        async fn strand(&self, server: &Server) -> TaskId {
            server.create_task("seen").await;
            let seen = server.full_replay().await.state().await;
            let missing = server.create_task("missing from the snapshot").await;
            let cursor = server.store.latest_seq().await.unwrap();
            self.device.bootstrapped_from_legacy(&seen, cursor).await;
            missing
        }

        async fn create_task(&self, title: &str, id: TaskId) {
            let mut task = NewTask::new(title);
            task.id = Some(id);
            self.local
                .dispatch(Command::CreateTask { task }, Actor::user())
                .await
                .unwrap();
        }

        async fn pending_outbox(&self) -> i64 {
            sqlx::query_as::<_, (i64,)>(
                "SELECT COUNT(*) FROM desktop_outbox WHERE flushed_at IS NULL",
            )
            .fetch_one(&self.device.pool)
            .await
            .unwrap()
            .0
        }

        /// Flush what is in the local log and not on the server, then echo.
        async fn echo_local_events(&self, server: &Server) -> CatchUpStats {
            let log = self.device.store.load_since(0, 1000).await.unwrap();
            for mut event in log {
                if server.store.load_by_id(event.id).await.unwrap().is_none() {
                    event.seq = 0;
                    let env = server.store.append(event).await.unwrap();
                    server.tasks.apply_event(&env).await.unwrap();
                }
            }
            self.device.replica.catch_up(server, 50).await.unwrap()
        }
    }

    #[tokio::test]
    async fn local_command_finished_during_repair_survives_it() {
        let server = Server::start().await;
        let file = ReplicaFile::create().await;
        let local_id = TaskId::new();
        let other = file.open_plain().await;
        let hook = {
            let process = file.open_plain().await;
            Box::pin(async move {
                process
                    .create_task("created while repair runs", local_id)
                    .await;
                assert!(process.device.tasks.get(local_id).await.unwrap().is_some());
            })
        };
        let syncing = file.open_hooked(hook).await;
        let missing = syncing.strand(&server).await;

        let stats = syncing.device.replica.catch_up(&server, 50).await.unwrap();

        assert_eq!(stats.fetched, 2);
        assert!(other.device.tasks.get(local_id).await.unwrap().is_some());
        assert!(other.device.tasks.get(missing).await.unwrap().is_some());
        assert_eq!(other.pending_outbox().await, 1, "still on its way out");
        assert!(other.device.replica.baseline_verified().await.unwrap());

        let stats = other.echo_local_events(&server).await;
        assert_eq!((stats.fetched, stats.applied), (1, 0));
        assert!(other.device.tasks.get(local_id).await.unwrap().is_some());
        assert_eq!(
            other.device.state().await,
            server.full_replay().await.state().await
        );
    }

    #[tokio::test]
    async fn local_command_stalled_before_its_apply_lands_on_the_repaired_rows() {
        let server = Server::start().await;
        let file = ReplicaFile::create().await;
        let syncing = file.open_plain().await;
        let store = Arc::new(Mutex::new(None));
        let writer = {
            let store = store.clone();
            let process = file
                .open(move |inner| {
                    let pausing = Arc::new(PausingStore {
                        inner,
                        armed: AtomicBool::new(true),
                        committed: Notify::new(),
                        resume: Notify::new(),
                    });
                    *store.try_lock().unwrap() = Some(pausing.clone());
                    pausing
                })
                .await;
            Arc::new(process)
        };
        let pausing = store.lock().await.take().unwrap();
        let missing = syncing.strand(&server).await;

        let local_id = TaskId::new();
        let command = tokio::spawn({
            let writer = writer.clone();
            async move { writer.create_task("stalled command", local_id).await }
        });
        pausing.committed.notified().await;

        syncing.device.replica.catch_up(&server, 50).await.unwrap();
        assert!(syncing.device.tasks.get(local_id).await.unwrap().is_some());
        assert!(syncing.device.tasks.get(missing).await.unwrap().is_some());

        pausing.resume.notify_one();
        command.await.unwrap();

        assert!(syncing.device.tasks.get(local_id).await.unwrap().is_some());
        assert_eq!(syncing.pending_outbox().await, 1);
        let stats = syncing.echo_local_events(&server).await;
        assert_eq!((stats.fetched, stats.applied), (1, 0));
        assert_eq!(
            syncing.device.state().await,
            server.full_replay().await.state().await
        );
    }

    #[tokio::test]
    async fn repair_waits_for_a_writer_holding_the_lock() {
        let server = Server::start().await;
        let file = ReplicaFile::create().await;
        let hook = {
            let writer = file.open_plain().await;
            Box::pin(async move {
                let mut tx = writer
                    .device
                    .pool
                    .begin_with("BEGIN IMMEDIATE")
                    .await
                    .unwrap();
                sqlx::query("UPDATE desktop_replica_state SET server_seq = server_seq")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    tx.commit().await.unwrap();
                    drop(writer);
                });
            })
        };
        let syncing = file.open_hooked(hook).await;
        let missing = syncing.strand(&server).await;

        syncing.device.replica.catch_up(&server, 50).await.unwrap();

        assert!(syncing.device.tasks.get(missing).await.unwrap().is_some());
        assert!(syncing.device.replica.baseline_verified().await.unwrap());
    }

    #[tokio::test]
    async fn slower_repair_does_not_undo_a_finished_one() {
        let server = Arc::new(Server::start().await);
        let file = ReplicaFile::create().await;
        let newer = TaskId::new();
        let hook = {
            let server = server.clone();
            let first = file.open_plain().await;
            Box::pin(async move {
                let mut task = NewTask::new("after the slow fold");
                task.id = Some(newer);
                server
                    .commands
                    .dispatch(Command::CreateTask { task }, Actor::user())
                    .await
                    .unwrap();
                first.device.replica.catch_up(&*server, 50).await.unwrap();
            })
        };
        let slow = file.open_hooked(hook).await;
        slow.strand(&server).await;

        let stats = slow.device.replica.catch_up(&*server, 50).await.unwrap();

        let cursor = server.store.latest_seq().await.unwrap();
        assert_eq!((stats.applied, stats.server_seq), (0, cursor));
        assert!(slow.device.tasks.get(newer).await.unwrap().is_some());
        assert_eq!(slow.device.replica.server_seq().await.unwrap(), cursor);
        assert_eq!(
            slow.device.state().await,
            server.full_replay().await.state().await
        );
    }

    #[tokio::test]
    async fn repair_never_moves_the_cursor_back() {
        let server = Server::start().await;
        let (device, _missing) = stranded_device(&server).await;
        let ahead = server.store.latest_seq().await.unwrap() + 5;
        advance_server_seq(&device.pool, ahead).await.unwrap();

        let stats = device.replica.catch_up(&server, 50).await.unwrap();

        assert_eq!(stats.server_seq, ahead);
        assert_eq!(device.replica.server_seq().await.unwrap(), ahead);
    }

    #[tokio::test]
    async fn repair_does_not_replay_an_event_the_server_already_has() {
        let server = Server::start().await;
        let (device, missing) = stranded_device(&server).await;
        let rename = |title: &str| Event::TaskUpdated {
            task_id: missing,
            patch: TaskPatch {
                title: Some(title.into()),
                ..TaskPatch::default()
            },
        };
        // A local edit reached the server, and the server moved on from it.
        let mut flushed = device.local_event(rename("local")).await;
        flushed.seq = 0;
        let env = server.store.append(flushed).await.unwrap();
        server.tasks.apply_event(&env).await.unwrap();
        let env = server
            .store
            .append(EventEnvelope::new(Actor::user(), rename("server")))
            .await
            .unwrap();
        server.tasks.apply_event(&env).await.unwrap();

        device.replica.catch_up(&server, 50).await.unwrap();

        let task = device.tasks.get(missing).await.unwrap().unwrap();
        assert_eq!(task.title, "server");
    }

    #[tokio::test]
    async fn repair_refuses_an_empty_server_log() {
        let server = Server::start().await;
        let device = fresh_device().await;
        device
            .bootstrapped_from_legacy(&ProjectionSnapshot::default(), 7)
            .await;
        device
            .local_event(Event::TaskCreated {
                task: NewTask::new("kept"),
            })
            .await;
        let before = device.state().await;

        assert!(device.replica.catch_up(&server, 50).await.is_err());
        assert_eq!(device.state().await, before);
        assert_eq!(device.replica.server_seq().await.unwrap(), 7);
    }

    #[tokio::test]
    async fn schema_upgrade_marks_an_existing_cursor_unverified() {
        let device = fresh_device().await;
        sqlx::query("DROP TABLE desktop_replica_state")
            .execute(&device.pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE desktop_replica_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                server_seq INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&device.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO desktop_replica_state (id, server_seq) VALUES (1, 9)")
            .execute(&device.pool)
            .await
            .unwrap();

        device.replica.ensure_schema().await.unwrap();
        device.replica.ensure_schema().await.unwrap();

        assert_eq!(device.replica.server_seq().await.unwrap(), 9);
        assert!(!device.replica.baseline_verified().await.unwrap());
    }

    #[tokio::test]
    async fn verified_baseline_is_kept_after_a_snapshot_bootstrap() {
        let server = Server::start().await;
        server.create_task("a").await;
        server.write_snapshot().await;
        server.create_task("b").await;

        let device = fresh_device().await;
        device.replica.catch_up(&server, 50).await.unwrap();

        assert!(device.replica.baseline_verified().await.unwrap());
        let stats = device.replica.catch_up(&server, 50).await.unwrap();
        assert_eq!(stats.fetched, 0, "no repair after a sound bootstrap");
    }

    // ── sync stopped between the log and the projectors ──────────────────────

    /// What a sync writes besides the rows `Device::state` compares: the feed
    /// and the task history, each comparable across replicas of one log.
    type Feed = Vec<(String, String, Option<String>, i64)>;
    type Versions = Vec<(String, i64, String, String)>;

    impl Device {
        async fn feed(&self) -> Feed {
            sqlx::query_as("SELECT event_id, verb, project_id, seq FROM activity ORDER BY seq")
                .fetch_all(&self.pool)
                .await
                .unwrap()
        }

        async fn versions(&self) -> Versions {
            sqlx::query_as(
                "SELECT entity_id, version_number, event_type, summary FROM entity_versions \
                 ORDER BY entity_id, version_number",
            )
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// Copies of `id` in the local log.
        async fn logged(&self, id: EventId) -> i64 {
            sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM events WHERE event_id = ?")
                .bind(id.to_string())
                .fetch_one(&self.pool)
                .await
                .unwrap()
                .0
        }

        /// Make the next write to `table` that satisfies `when` fail. The real
        /// sync path runs up to that statement.
        async fn stop_sync_at(&self, table: &str, when: &str) {
            let trigger = format!(
                "CREATE TRIGGER stop_sync BEFORE INSERT ON {table} WHEN {when} \
                 BEGIN SELECT RAISE(ABORT, 'sync stopped'); END"
            );
            sqlx::query(&trigger).execute(&self.pool).await.unwrap();
        }

        async fn let_sync_through(&self) {
            sqlx::query("DROP TRIGGER stop_sync")
                .execute(&self.pool)
                .await
                .unwrap();
        }
    }

    impl Server {
        /// Put `payload` in the log, as a device or a command would.
        async fn log(&self, payload: Event) -> EventEnvelope {
            let env = self
                .store
                .append(EventEnvelope::new(Actor::user(), payload))
                .await
                .unwrap();
            self.tasks.apply_event(&env).await.unwrap();
            env
        }

        async fn tip(&self) -> u64 {
            self.store.latest_seq().await.unwrap()
        }
    }

    fn comment_on(task_id: TaskId, body: &str) -> Event {
        Event::CommentAdded {
            comment: Comment::from_new(
                NewComment {
                    id: None,
                    task_id,
                    body: body.into(),
                    parent_id: None,
                    kind: None,
                },
                Actor::user(),
                chrono::Utc::now(),
            ),
        }
    }

    fn rename(task_id: TaskId, title: &str) -> Event {
        Event::TaskUpdated {
            task_id,
            patch: TaskPatch {
                title: Some(title.into()),
                ..TaskPatch::default()
            },
        }
    }

    /// A log that touches every projector: project, task, its history, a
    /// comment, and a command whose events merge in the feed.
    async fn log_of_every_kind(server: &Server) -> TaskId {
        let project = Project::new("demo", None);
        let mut task = NewTask::new("in the project");
        let id = TaskId::new();
        task.id = Some(id);
        task.project_id = Some(project.id);
        server.log(Event::ProjectCreated { project }).await;
        server.log(Event::TaskCreated { task }).await;
        server.log(comment_on(id, "first")).await;
        server
            .commands
            .dispatch(Command::CompleteTask { id, note: None }, Actor::user())
            .await
            .unwrap();
        id
    }

    async fn assert_matches_full_replay(device: &Device, server: &Server) {
        let reference = server.full_replay().await;
        assert_eq!(device.state().await, reference.state().await);
        assert_eq!(device.feed().await, reference.feed().await);
        assert_eq!(device.versions().await, reference.versions().await);
        assert_eq!(
            device.replica.server_seq().await.unwrap(),
            server.tip().await
        );
    }

    #[tokio::test]
    async fn sync_stopped_right_after_the_append_recovers_on_restart() {
        let server = Server::start().await;
        let file = ReplicaFile::create().await;
        let process = file.open_plain().await;
        server.create_task("seen").await;
        process.device.replica.catch_up(&server, 50).await.unwrap();

        let task = server.create_task("E").await;
        let event = server.store.load_since(1, 1).await.unwrap().remove(0);
        // The first projector is the first write after the append.
        process.device.stop_sync_at("tasks", "1").await;
        assert!(process.device.replica.catch_up(&server, 50).await.is_err());
        process.device.let_sync_through().await;
        drop(process);

        let restarted = file.open_plain().await;
        let stats = restarted
            .device
            .replica
            .catch_up(&server, 50)
            .await
            .unwrap();

        assert!(
            restarted.device.tasks.get(task).await.unwrap().is_some(),
            "the fetch after a restart must bring E to the tasks table"
        );
        assert_eq!(stats.applied, 1);
        assert_eq!(restarted.device.logged(event.id).await, 1);
        assert_matches_full_replay(&restarted.device, &server).await;
    }

    #[tokio::test]
    async fn sync_stopped_anywhere_leaves_nothing_behind_and_retries_match_a_replay() {
        for (table, when) in [
            ("tasks", "1"),
            ("entity_versions", "1"),
            ("projects", "1"),
            ("comments", "1"),
            ("activity", "1"),
            ("activity", "NEW.verb = 'commented'"),
            ("activity", "NEW.verb = 'status_changed'"),
        ] {
            let server = Server::start().await;
            log_of_every_kind(&server).await;
            let file = ReplicaFile::create().await;
            let process = file.open_plain().await;

            process.device.stop_sync_at(table, when).await;
            for _ in 0..3 {
                assert!(
                    process.device.replica.catch_up(&server, 50).await.is_err(),
                    "{table} {when}"
                );
            }
            let cursor = process.device.replica.server_seq().await.unwrap();
            let log = process.device.store.load_since(0, 1000).await.unwrap();
            assert_eq!(log.last().map_or(0, |e| e.seq), cursor, "{table} {when}");
            process.device.let_sync_through().await;
            drop(process);

            let restarted = file.open_plain().await;
            restarted
                .device
                .replica
                .catch_up(&server, 50)
                .await
                .unwrap();
            assert_matches_full_replay(&restarted.device, &server).await;

            let stats = restarted
                .device
                .replica
                .catch_up(&server, 50)
                .await
                .unwrap();
            assert_eq!((stats.fetched, stats.applied), (0, 0), "{table} {when}");
            assert_matches_full_replay(&restarted.device, &server).await;
        }
    }

    #[tokio::test]
    async fn retry_does_not_roll_back_a_newer_change() {
        let server = Server::start().await;
        let task = server.create_task("one").await;
        server.log(rename(task, "two")).await;
        server.log(rename(task, "three")).await;
        let device = fresh_device().await;

        device
            .stop_sync_at("activity", "NEW.verb = 'updated'")
            .await;
        assert!(device.replica.catch_up(&server, 50).await.is_err());
        let held = device.tasks.get(task).await.unwrap().unwrap();
        assert_eq!(held.title, "one", "the failed edit left no trace");

        device.let_sync_through().await;
        device.replica.catch_up(&server, 50).await.unwrap();
        device.replica.catch_up(&server, 50).await.unwrap();

        let task = device.tasks.get(task).await.unwrap().unwrap();
        assert_eq!(task.title, "three");
        assert_matches_full_replay(&device, &server).await;
    }

    #[tokio::test]
    async fn failed_event_holds_the_cursor_and_everything_behind_it() {
        let server = Server::start().await;
        let done = server.create_task("before").await;
        let cursor = server.tip().await;
        let failing = server.create_task("held up").await;
        let held_event = server.store.load_since(cursor, 1).await.unwrap().remove(0);
        let later = server.create_task("after").await;
        let later_event = server
            .store
            .load_since(cursor + 1, 1)
            .await
            .unwrap()
            .remove(0);
        let device = fresh_device().await;

        device.stop_sync_at("tasks", "NEW.title = 'held up'").await;
        for _ in 0..2 {
            assert!(device.replica.catch_up(&server, 50).await.is_err());
            assert_eq!(device.replica.server_seq().await.unwrap(), cursor);
            assert!(device.tasks.get(done).await.unwrap().is_some());
            assert!(device.tasks.get(failing).await.unwrap().is_none());
            assert!(device.tasks.get(later).await.unwrap().is_none());
            assert_eq!(device.logged(held_event.id).await, 0);
            assert_eq!(device.logged(later_event.id).await, 0);
        }

        device.let_sync_through().await;
        let stats = device.replica.catch_up(&server, 50).await.unwrap();

        assert_eq!((stats.fetched, stats.applied), (2, 2));
        assert_eq!(device.logged(held_event.id).await, 1);
        assert_eq!(device.logged(later_event.id).await, 1);
        assert_matches_full_replay(&device, &server).await;
    }

    #[tokio::test]
    async fn catch_up_in_small_pages_recovers_from_a_stop() {
        let server = Server::start().await;
        log_of_every_kind(&server).await;
        let device = fresh_device().await;

        // The second event of the first page.
        device
            .stop_sync_at("activity", "NEW.verb = 'created'")
            .await;
        assert!(device.replica.catch_up(&server, 2).await.is_err());
        assert_eq!(device.replica.server_seq().await.unwrap(), 1);
        device.let_sync_through().await;
        catch_up_fully(&device, &server).await;

        assert_matches_full_replay(&device, &server).await;
    }

    #[tokio::test]
    async fn echo_of_a_local_command_only_moves_the_cursor() {
        let server = Server::start().await;
        let file = ReplicaFile::create().await;
        let process = file.open_plain().await;
        server.create_task("seen").await;
        process.device.replica.catch_up(&server, 50).await.unwrap();
        let local = TaskId::new();
        process.create_task("local", local).await;
        let feed = process.device.feed().await;
        let versions = process.device.versions().await;

        let stats = process.echo_local_events(&server).await;

        assert_eq!((stats.fetched, stats.applied), (1, 0));
        assert_eq!(stats.server_seq, server.tip().await);
        assert_eq!(process.device.feed().await, feed);
        assert_eq!(process.device.versions().await, versions);
        assert_eq!(
            process.device.state().await,
            server.full_replay().await.state().await
        );
    }

    #[tokio::test]
    async fn two_syncs_of_one_device_apply_each_event_once() {
        let server = Server::start().await;
        for n in 0..20 {
            server.create_task(&format!("task {n}")).await;
        }
        log_of_every_kind(&server).await;
        let file = ReplicaFile::create().await;
        let (first, second) = (file.open_plain().await, file.open_plain().await);

        let (a, b) = tokio::join!(
            first.device.replica.catch_up(&server, 100),
            second.device.replica.catch_up(&server, 100),
        );

        a.unwrap();
        b.unwrap();
        for event in server.store.load_since(0, 100).await.unwrap() {
            assert_eq!(first.device.logged(event.id).await, 1);
        }
        assert_eq!(
            first.device.state().await,
            server.full_replay().await.state().await
        );
        assert_eq!(
            first.device.replica.server_seq().await.unwrap(),
            server.tip().await
        );
    }

    /// What the old sync did to a batch, with `lost` events stopped between
    /// the log and the projectors and then skipped by their id.
    async fn sync_like_the_old_binary(device: &Device, server: &Server, lost: &[EventId]) {
        let since = device.replica.server_seq().await.unwrap();
        for mut event in server.fetch_events(since, 1000).await.unwrap() {
            event.seq = 0;
            let env = device.store.append(event).await.unwrap();
            if !lost.contains(&env.id) {
                device.tasks.apply_event(&env).await.unwrap();
                device.projects.apply_event(&env).await.unwrap();
                device.comments.apply_event(&env).await.unwrap();
                device.replica.activity.apply_event(&env).await.unwrap();
            }
        }
        advance_server_seq(&device.pool, server.tip().await)
            .await
            .unwrap();
    }

    /// Reopen the file the way the upgraded binary finds a database of the old
    /// one, which has no idea whether its log was projected.
    async fn upgraded(file: &ReplicaFile, process: Process) -> Process {
        sqlx::query("ALTER TABLE desktop_replica_state DROP COLUMN projections_verified")
            .execute(&process.device.pool)
            .await
            .unwrap();
        drop(process);
        file.open_plain().await
    }

    /// The feed a replay of this device's local log gives.
    async fn feed_of_the_local_log(device: &Device) -> Feed {
        let reference = fresh_device().await;
        let log = device.store.load_since(0, 1000).await.unwrap();
        reference.replica.apply_remote_events(log).await.unwrap();
        reference.feed().await
    }

    #[tokio::test]
    async fn replica_damaged_by_the_old_sync_is_repaired_behind_its_cursor() {
        let server = Server::start().await;
        let seen = server.create_task("seen").await;
        let file = ReplicaFile::create().await;
        let process = file.open_plain().await;
        process.device.replica.catch_up(&server, 50).await.unwrap();

        // Work of this device that the server has not got yet.
        let offline = TaskId::new();
        process.create_task("created offline", offline).await;
        process
            .local
            .dispatch(
                Command::CompleteTask {
                    id: seen,
                    note: None,
                },
                Actor::user(),
            )
            .await
            .unwrap();
        let pending = process.pending_outbox().await;

        let lost_task = server.create_task("E").await;
        let cursor = server.tip().await - 1;
        let lost_event = server.store.load_since(cursor, 1).await.unwrap().remove(0);
        server.log(rename(lost_task, "E, renamed")).await;
        let lost_comment = server.log(comment_on(seen, "lost")).await;
        sync_like_the_old_binary(&process.device, &server, &[lost_event.id, lost_comment.id]).await;
        let device = &process.device;
        assert!(device.tasks.get(lost_task).await.unwrap().is_none());
        assert!(device.comments.list_all().await.unwrap().is_empty());
        let damaged_cursor = device.replica.server_seq().await.unwrap();
        assert!(damaged_cursor >= lost_event.seq);

        let process = upgraded(&file, process).await;
        let newer = server.create_task("after the upgrade").await;
        process.device.replica.catch_up(&server, 50).await.unwrap();
        let device = &process.device;

        let task = device.tasks.get(lost_task).await.unwrap().unwrap();
        assert_eq!(task.title, "E, renamed", "the later edit counts as well");
        assert_eq!(device.comments.list_all().await.unwrap().len(), 1);
        assert_eq!(device.logged(lost_event.id).await, 1);
        assert!(device.tasks.get(newer).await.unwrap().is_some());
        assert_eq!(
            device.replica.server_seq().await.unwrap(),
            server.tip().await
        );

        // This device's work stayed and is still on its way.
        assert!(device.tasks.get(offline).await.unwrap().is_some());
        let task = device.tasks.get(seen).await.unwrap().unwrap();
        assert_eq!(task.status, Status::Done);
        assert_eq!(process.pending_outbox().await, pending);
        assert_eq!(device.feed().await, feed_of_the_local_log(device).await);
        let history: Vec<_> = device
            .versions()
            .await
            .into_iter()
            .filter(|version| version.0 == lost_task.to_string())
            .map(|version| version.2)
            .collect();
        assert_eq!(history, ["task_created", "task_updated"]);

        // Once only: the next round is an ordinary delta.
        let stats = device.replica.catch_up(&server, 50).await.unwrap();
        assert_eq!(stats.fetched, 0);

        process.echo_local_events(&server).await;
        let reference = server.full_replay().await;
        assert_eq!(device.state().await, reference.state().await);
        assert_eq!(device.versions().await, reference.versions().await);
    }

    #[tokio::test]
    async fn first_sync_stopped_by_the_old_binary_is_repaired_at_cursor_zero() {
        let server = Server::start().await;
        let task = server.create_task("E").await;
        let event = server.store.load_since(0, 1).await.unwrap().remove(0);
        server.log(rename(task, "E, renamed")).await;
        let file = ReplicaFile::create().await;
        let process = file.open_plain().await;
        // The old batch wrote its cursor last, so stopping leaves it at 0.
        process
            .device
            .store
            .append(EventEnvelope {
                seq: 0,
                ..event.clone()
            })
            .await
            .unwrap();
        // The baseline did go in before the first fetch.
        process
            .device
            .replica
            .set_verified_baseline(0)
            .await
            .unwrap();

        let process = upgraded(&file, process).await;
        process.device.replica.catch_up(&server, 50).await.unwrap();

        assert_eq!(process.device.logged(event.id).await, 1);
        assert_matches_full_replay(&process.device, &server).await;
    }

    #[tokio::test]
    async fn damaged_replica_on_a_snapshot_is_repaired_too() {
        let server = Server::start().await;
        server.create_task("in the snapshot").await;
        server.write_snapshot().await;
        let file = ReplicaFile::create().await;
        let process = file.open_plain().await;
        process.device.replica.catch_up(&server, 50).await.unwrap();

        let lost_task = server.create_task("E").await;
        let lost = server
            .store
            .load_since(server.tip().await - 1, 1)
            .await
            .unwrap()
            .remove(0);
        sync_like_the_old_binary(&process.device, &server, &[lost.id]).await;

        let process = upgraded(&file, process).await;
        process.device.replica.catch_up(&server, 50).await.unwrap();
        let device = &process.device;

        assert!(device.tasks.get(lost_task).await.unwrap().is_some());
        assert_eq!(
            device.state().await,
            server.full_replay().await.state().await
        );
        assert_eq!(device.feed().await, feed_of_the_local_log(device).await);
    }

    #[tokio::test]
    async fn device_that_only_worked_offline_keeps_its_work_through_the_upgrade() {
        let server = Server::start().await;
        server.create_task("on the server").await;
        let file = ReplicaFile::create().await;
        let process = file.open_plain().await;
        let offline = TaskId::new();
        process.create_task("created offline", offline).await;

        let process = upgraded(&file, process).await;
        process.device.replica.catch_up(&server, 50).await.unwrap();

        assert!(process.device.tasks.get(offline).await.unwrap().is_some());
        assert_eq!(process.pending_outbox().await, 1);
        process.echo_local_events(&server).await;
        assert_eq!(
            process.device.state().await,
            server.full_replay().await.state().await
        );
    }
}
