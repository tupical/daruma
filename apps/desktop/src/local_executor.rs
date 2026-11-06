//! Optimistic local command execution backed by the offline outbox.

use daruma_core::embed::{Command, CommandBus, EventEnvelope};
use daruma_domain::Actor;
use daruma_shared::Result;

use crate::{flush, flush::RemoteEventSink, outbox::Outbox, replica::ReplicaSource};

pub struct LocalExecutor {
    commands: CommandBus,
    outbox: Outbox,
}

impl LocalExecutor {
    /// `commands` must write through [`Outbox::outbound_store`]: that is what
    /// queues the events of a command.
    pub fn new(commands: CommandBus, outbox: Outbox) -> Self {
        Self { commands, outbox }
    }

    /// An error from the commit leaves nothing behind. One from the projectors
    /// leaves the command committed and queued, and the next sync redoes the
    /// rows.
    pub async fn dispatch(&self, cmd: Command, actor: Actor) -> Result<Vec<EventEnvelope>> {
        let envelopes = self.commands.dispatch(cmd, actor).await?;
        // The command is done either way: at worst a sync redoes the rows.
        if let Err(error) = self.outbox.mark_projected(&envelopes).await {
            tracing::warn!(%error, "could not record that a command was projected");
        }
        Ok(envelopes)
    }

    /// Queue what an older binary left out of the outbox; run it before a
    /// flush, so a restored event goes out ahead of the ones that followed it.
    pub async fn restore_lost(&self, remote: &dyn ReplicaSource, limit: u32) -> Result<usize> {
        self.outbox.restore_lost(remote, limit).await
    }

    pub async fn flush_pending(
        &self,
        sink: &dyn RemoteEventSink,
        limit: u32,
    ) -> Result<flush::FlushStats> {
        flush::flush_pending(&self.outbox, sink, limit).await
    }

    /// Put every dead letter back in the queue; how many came back.
    pub async fn requeue_rejected(&self) -> Result<u64> {
        self.outbox.requeue_rejected().await
    }

    /// Events the server refused for good (dead letters), all time.
    pub async fn rejected_count(&self) -> Result<u64> {
        self.outbox.rejected_count().await
    }

    #[cfg(test)]
    async fn pending_outbox_len(&self) -> Result<usize> {
        self.outbox.pending(u32::MAX).await.map(|items| items.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_core::{
        embed::{ActivityRepo, CommentRepo, Db, EventBus, TaskRepo},
        CommandHandler,
    };
    use daruma_domain::NewTask;
    use std::sync::Arc;

    #[tokio::test]
    async fn dispatch_applies_locally_and_enqueues_events() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let pool = db.pool().clone();
        let tasks = Arc::new(TaskRepo::new(pool.clone()));
        let projects = Arc::new(daruma_core::embed::ProjectRepo::new(pool.clone()));
        let comments = Arc::new(CommentRepo::new(pool.clone()));
        let activity = Arc::new(ActivityRepo::new(pool));
        let bus = EventBus::new(16);
        let outbox = Outbox::new(db);
        outbox.ensure_schema().await.unwrap();
        let commands = CommandBus::new(Arc::new(CommandHandler::new(
            Arc::new(outbox.outbound_store().await.unwrap()),
            tasks.clone(),
            projects,
            comments,
            activity,
            bus,
        )));
        let executor = LocalExecutor::new(commands, outbox);

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
