//! Optimistic local command execution backed by the offline outbox.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use taskagent_core::embed::{Command, CommandBus, EventEnvelope};
use taskagent_domain::Actor;
use taskagent_shared::{DeviceId, Result};

use crate::{flush, flush::RemoteEventSink, outbox::Outbox};

pub struct LocalExecutor {
    commands: CommandBus,
    outbox: Outbox,
    origin_device_id: DeviceId,
    next_origin_seq: Arc<AtomicU64>,
}

impl LocalExecutor {
    pub async fn new(commands: CommandBus, outbox: Outbox) -> Result<Self> {
        let origin_device_id = DeviceId::new();
        let next_origin_seq = outbox.next_origin_seq(origin_device_id).await?;
        Ok(Self {
            commands,
            outbox,
            origin_device_id,
            next_origin_seq: Arc::new(AtomicU64::new(next_origin_seq)),
        })
    }

    pub async fn dispatch(&self, cmd: Command, actor: Actor) -> Result<Vec<EventEnvelope>> {
        let envelopes = self.commands.dispatch(cmd, actor).await?;
        for envelope in &envelopes {
            let origin_seq = self.next_origin_seq.fetch_add(1, Ordering::SeqCst);
            self.outbox
                .enqueue(self.origin_device_id, origin_seq, envelope.clone())
                .await?;
        }
        Ok(envelopes)
    }

    pub async fn flush_pending(
        &self,
        sink: &dyn RemoteEventSink,
        limit: u32,
    ) -> Result<flush::FlushStats> {
        flush::flush_pending(&self.outbox, sink, limit).await
    }

    #[cfg(test)]
    async fn pending_outbox_len(&self) -> Result<usize> {
        self.outbox.pending(u32::MAX).await.map(|items| items.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use taskagent_core::{
        embed::{ActivityRepo, CommentRepo, Db, EventBus, SqliteEventStore, TaskRepo},
        CommandHandler,
    };
    use taskagent_domain::NewTask;

    #[tokio::test]
    async fn dispatch_applies_locally_and_enqueues_events() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let pool = db.pool().clone();
        let store = Arc::new(SqliteEventStore::new(pool.clone()));
        let tasks = Arc::new(TaskRepo::new(pool.clone()));
        let projects = Arc::new(taskagent_core::embed::ProjectRepo::new(pool.clone()));
        let comments = Arc::new(CommentRepo::new(pool.clone()));
        let activity = Arc::new(ActivityRepo::new(pool));
        let bus = EventBus::new(16);
        let commands = CommandBus::new(Arc::new(CommandHandler::new(
            store,
            tasks.clone(),
            projects,
            comments,
            activity,
            bus,
        )));
        let outbox = Outbox::new(db);
        outbox.ensure_schema().await.unwrap();
        let executor = LocalExecutor::new(commands, outbox).await.unwrap();

        let events = executor
            .dispatch(
                Command::CreateTask {
                    task: NewTask::new("offline local"),
                },
                Actor::user(),
            )
            .await
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(tasks.list_all().await.unwrap().len(), 1);
        assert_eq!(executor.pending_outbox_len().await.unwrap(), 1);
    }
}
