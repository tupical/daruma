//! Local SQLite replica catch-up from the server event log.

use std::sync::Arc;

use sqlx::SqlitePool;
use daruma_core::embed::{
    ActivityRepo, CommentRepo, EventEnvelope, EventStore, ProjectRepo, ProjectionSnapshot,
    Snapshot, TaskRepo,
};
use daruma_shared::{CoreError, Result};

use crate::remote::HttpReplicaSink;

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

    pub async fn catch_up(&self, remote: &HttpReplicaSink, limit: u32) -> Result<CatchUpStats> {
        let mut since = self.server_seq().await?;
        if since == 0 {
            // First catch-up on this device: try to bootstrap from the
            // server's materialised projection snapshot instead of replaying
            // the whole log from seq 0. No snapshot (or an older server
            // without the endpoint) → old full-replay path unchanged.
            if let Some(snapshot) = remote.fetch_snapshot().await? {
                tracing::info!(
                    seq = snapshot.seq,
                    tasks = snapshot.payload.tasks.len(),
                    projects = snapshot.payload.projects.len(),
                    comments = snapshot.payload.comments.len(),
                    "bootstrapping replica from server snapshot"
                );
                self.bootstrap_from_snapshot(&snapshot).await?;
                since = snapshot.seq;
            }
        }
        let events = remote.fetch_events(since, limit).await?;
        self.apply_remote_events(events).await
    }

    /// Bootstrap from a server snapshot: restore the projection state and
    /// move the server cursor to `snapshot.seq`, so the next fetch replays
    /// only the delta. Restore goes through the same upsert SQL the
    /// projectors use, so re-applying events the snapshot already reflects
    /// is a no-op.
    pub async fn bootstrap_from_snapshot(&self, snapshot: &Snapshot) -> Result<()> {
        self.restore_snapshot(&snapshot.payload).await?;
        self.set_server_seq(snapshot.seq).await
    }

    async fn restore_snapshot(&self, payload: &ProjectionSnapshot) -> Result<()> {
        // Restore parents before children (projects → tasks → comments) to
        // match the replay order a full catch-up would produce.
        for project in &payload.projects {
            self.projects.upsert_project(project).await?;
        }
        for task in &payload.tasks {
            self.tasks.upsert_task(task).await?;
        }
        for comment in &payload.comments {
            self.comments.upsert_comment(comment).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_core::embed::{Db, Event, SnapshotRepo, SqliteEventStore};
    use daruma_domain::{
        Actor, Comment, CommentPatch, NewComment, NewTask, Project, Status, Task, TaskPatch,
    };

    struct Device {
        replica: Replica,
        tasks: Arc<TaskRepo>,
        projects: Arc<ProjectRepo>,
        comments: Arc<CommentRepo>,
    }

    async fn fresh_device() -> Device {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let pool = db.pool().clone();
        let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
        let tasks = Arc::new(TaskRepo::new(pool.clone()));
        let projects = Arc::new(ProjectRepo::new(pool.clone()));
        let comments = Arc::new(CommentRepo::new(pool.clone()));
        let activity = Arc::new(ActivityRepo::new(pool.clone()));
        let replica = Replica::new(
            pool,
            store,
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

        let stats = device.replica.apply_remote_events(vec![event]).await.unwrap();
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

        // Pre-snapshot history.
        let project = Project::new("demo", None);
        let project_id = project.id;
        let mut new_t1 = NewTask::new("t1");
        new_t1.project_id = Some(project_id);
        let t1 = new_t1.id.unwrap_or_default();
        let t2 = NewTask::new("t2").id.unwrap_or_default();
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
            Event::TaskCreated {
                task: NewTask::new("t2"),
            },
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
        let snapshot_seq = s_store.latest_seq().await.unwrap();
        let payload = ProjectionSnapshot {
            tasks: s_tasks.list_all().await.unwrap(),
            projects: s_projects.list_all().await.unwrap(),
            comments: s_comments.list_all().await.unwrap(),
        };
        let snapshot = s_snapshots.insert(snapshot_seq, &payload).await.unwrap();

        // Post-snapshot delta.
        for payload in [
            Event::TaskCreated {
                task: NewTask::new("t3"),
            },
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

        // `updated_event_seq` tracks the replica-local log position; a
        // snapshot-bootstrapped replica has no local rows for pre-snapshot
        // events, so normalise it before comparing (see module docs).
        fn normalized(mut tasks: Vec<Task>) -> Vec<Task> {
            for t in &mut tasks {
                t.updated_event_seq = None;
            }
            tasks
        }
        assert_eq!(
            normalized(a.tasks.list_all().await.unwrap()),
            normalized(b.tasks.list_all().await.unwrap()),
        );
        assert_eq!(
            a.projects.list_all().await.unwrap(),
            b.projects.list_all().await.unwrap(),
        );
        assert_eq!(
            a.comments.list_all().await.unwrap(),
            b.comments.list_all().await.unwrap(),
        );

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
}
