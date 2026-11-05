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
        // 1 once the cursor is known to rest on a replay from seq 0 or on a
        // consistent snapshot. A cursor written before this column existed
        // may rest on a snapshot that missed events, and the column default
        // sends such a replica through `rebuild_from_log`.
        let (has_baseline,) = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM pragma_table_info('desktop_replica_state') \
             WHERE name = 'baseline_verified'",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        if has_baseline == 0 {
            sqlx::query(
                "ALTER TABLE desktop_replica_state \
                 ADD COLUMN baseline_verified INTEGER NOT NULL DEFAULT 0",
            )
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
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

    async fn set_server_seq(&self, server_seq: u64) -> Result<()> {
        sqlx::query("UPDATE desktop_replica_state SET server_seq = ? WHERE id = 1")
            .bind(server_seq as i64)
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    async fn baseline_verified(&self) -> Result<bool> {
        let row = sqlx::query_as::<_, (i64,)>(
            "SELECT baseline_verified FROM desktop_replica_state WHERE id = 1",
        )
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
        let mut since = self.server_seq().await?;
        if since > 0 && !self.baseline_verified().await? {
            return self.rebuild_from_log(remote, limit).await;
        }
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
        self.apply_remote_events(events).await
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

    /// One-time repair for a cursor that may rest on a snapshot which missed
    /// events: those events sit at or below the cursor, so no delta can bring
    /// them back. Rebuild the projections as the server log plus whatever only
    /// this device has (events still in the outbox or refused by the server).
    ///
    /// Everything is folded on the side, so a failed fetch leaves the replica
    /// untouched. Other processes of this device keep writing meanwhile: the
    /// rows and the baseline change in one write transaction, which first
    /// reads whatever reached the local log since. A command that committed
    /// before it is in the fold, and any later one lands on the new rows. A
    /// crash rolls back, and the next sync reruns the repair.
    async fn rebuild_from_log(
        &self,
        remote: &dyn ReplicaSource,
        limit: u32,
    ) -> Result<CatchUpStats> {
        let cursor = self.server_seq().await?;
        let fold = ProjectionFold::new(None).await?;
        let mut server_ids = HashSet::new();
        let mut since = 0;
        loop {
            let page = remote.fetch_events(since, limit).await?;
            let Some(last) = page.last() else { break };
            since = last.seq;
            for event in &page {
                fold.apply(event).await?;
                server_ids.insert(event.id);
            }
        }
        if server_ids.is_empty() {
            // An empty log cannot be the one this cursor came from, and
            // adopting it would wipe the replica.
            return Err(CoreError::sync(format!(
                "server log is empty but the replica cursor is at {cursor}"
            )));
        }
        tracing::info!(
            cursor,
            server_seq = since,
            events = server_ids.len(),
            "rebuilding replica from the server log"
        );
        // The bulk of the local log goes in before the lock is taken, so
        // writers only ever wait for the tail and the row swap.
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

        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let (current, verified) = sqlx::query_as::<_, (i64, i64)>(
            "SELECT server_seq, baseline_verified FROM desktop_replica_state WHERE id = 1",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        let current = current.max(0) as u64;
        if verified != 0 {
            // Another sync of this device finished the repair, maybe from a
            // newer server log than the one folded here.
            return Ok(CatchUpStats {
                fetched: server_ids.len(),
                applied: 0,
                server_seq: current,
            });
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
        let server_seq = since.max(current);
        mark_verified_baseline(&mut *tx, server_seq).await?;
        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(CatchUpStats {
            fetched: server_ids.len(),
            applied: server_ids.len(),
            server_seq,
        })
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

        for mut event in events {
            let server_seq = event.seq;
            max_server_seq = max_server_seq.max(server_seq);
            if self.store.load_by_id(event.id).await?.is_some() {
                continue;
            }
            event.seq = 0;
            let persisted = self.store.append(event).await?;
            self.apply_projection(&persisted).await?;
            applied += 1;
        }

        self.set_server_seq(max_server_seq).await?;
        Ok(CatchUpStats {
            fetched,
            applied,
            server_seq: max_server_seq,
        })
    }

    async fn apply_projection(&self, event: &EventEnvelope) -> Result<()> {
        self.tasks.apply_event(event).await?;
        self.projects.apply_event(event).await?;
        self.comments.apply_event(event).await?;
        self.activity.apply_event(event).await?;
        Ok(())
    }
}

async fn mark_verified_baseline<'e, E: SqliteExecutor<'e>>(
    executor: E,
    server_seq: u64,
) -> Result<()> {
    sqlx::query(
        "UPDATE desktop_replica_state SET server_seq = ?, baseline_verified = 1 WHERE id = 1",
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
            self.replica.set_server_seq(seq).await.unwrap();
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
}
