//! Local SQLite replica catch-up from the server event log.

use std::sync::Arc;

use sqlx::SqlitePool;
use daruma_core::embed::{
    ActivityRepo, CommentRepo, EventEnvelope, EventStore, ProjectRepo, TaskRepo,
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
        let since = self.server_seq().await?;
        let events = remote.fetch_events(since, limit).await?;
        self.apply_remote_events(events).await
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
    use daruma_core::embed::{Db, Event, SqliteEventStore};
    use daruma_domain::{Actor, NewTask};

    #[tokio::test]
    async fn applies_remote_events_once_and_advances_server_cursor() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let pool = db.pool().clone();
        let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
        let tasks = Arc::new(TaskRepo::new(pool.clone()));
        let projects = Arc::new(ProjectRepo::new(pool.clone()));
        let comments = Arc::new(CommentRepo::new(pool.clone()));
        let activity = Arc::new(ActivityRepo::new(pool.clone()));
        let replica = Replica::new(pool, store, tasks.clone(), projects, comments, activity);
        replica.ensure_schema().await.unwrap();

        let mut event = EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated {
                task: NewTask::new("remote"),
            },
        );
        event.seq = 7;

        let stats = replica
            .apply_remote_events(vec![event.clone()])
            .await
            .unwrap();
        assert_eq!(stats.applied, 1);
        assert_eq!(stats.server_seq, 7);
        assert_eq!(tasks.list_all().await.unwrap().len(), 1);

        let stats = replica.apply_remote_events(vec![event]).await.unwrap();
        assert_eq!(stats.applied, 0);
        assert_eq!(stats.server_seq, 7);
        assert_eq!(tasks.list_all().await.unwrap().len(), 1);
    }
}
