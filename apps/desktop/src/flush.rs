//! Reconnect flush loop for pending desktop outbox events.

#![allow(dead_code)] // Transport hook is wired in the next Phase 2 block.

use async_trait::async_trait;
use taskagent_core::embed::EventEnvelope;
use taskagent_shared::Result;

use crate::outbox::Outbox;

#[async_trait]
pub trait RemoteEventSink: Send + Sync {
    async fn push(&self, envelope: EventEnvelope) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlushStats {
    pub attempted: usize,
    pub flushed: usize,
}

pub async fn flush_pending(
    outbox: &Outbox,
    sink: &dyn RemoteEventSink,
    limit: u32,
) -> Result<FlushStats> {
    let pending = outbox.pending(limit).await?;
    let mut flushed = 0usize;
    for entry in &pending {
        sink.push(entry.envelope.clone()).await?;
        if outbox.mark_flushed(entry.id).await? {
            flushed += 1;
        }
    }
    Ok(FlushStats {
        attempted: pending.len(),
        flushed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use taskagent_core::embed::{Db, Event, EventEnvelope};
    use taskagent_domain::{Actor, NewTask};
    use taskagent_shared::{CoreError, DeviceId};

    struct RecordingSink {
        seen: Arc<Mutex<Vec<u64>>>,
        fail_on_seq: Option<u64>,
    }

    #[async_trait]
    impl RemoteEventSink for RecordingSink {
        async fn push(&self, envelope: EventEnvelope) -> Result<()> {
            if Some(envelope.origin_seq) == self.fail_on_seq {
                return Err(CoreError::sync("remote unavailable"));
            }
            self.seen.lock().unwrap().push(envelope.origin_seq);
            Ok(())
        }
    }

    fn envelope(title: &str) -> EventEnvelope {
        EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated {
                task: NewTask::new(title),
            },
        )
    }

    #[tokio::test]
    async fn flushes_in_origin_order_and_marks_after_success() {
        let db = Db::memory().await.unwrap();
        let outbox = Outbox::new(db);
        outbox.ensure_schema().await.unwrap();
        let device = DeviceId::new();
        outbox.enqueue(device, 1, envelope("one")).await.unwrap();
        outbox.enqueue(device, 2, envelope("two")).await.unwrap();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = RecordingSink {
            seen: seen.clone(),
            fail_on_seq: None,
        };
        let stats = flush_pending(&outbox, &sink, 100).await.unwrap();

        assert_eq!(stats.flushed, 2);
        assert_eq!(*seen.lock().unwrap(), vec![1, 2]);
        assert!(outbox.pending(100).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_push_leaves_event_pending() {
        let db = Db::memory().await.unwrap();
        let outbox = Outbox::new(db);
        outbox.ensure_schema().await.unwrap();
        let device = DeviceId::new();
        outbox.enqueue(device, 1, envelope("one")).await.unwrap();

        let sink = RecordingSink {
            seen: Arc::new(Mutex::new(Vec::new())),
            fail_on_seq: Some(1),
        };

        assert!(flush_pending(&outbox, &sink, 100).await.is_err());
        assert_eq!(outbox.pending(100).await.unwrap().len(), 1);
    }
}
