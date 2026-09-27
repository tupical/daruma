//! Reconnect flush loop for pending desktop outbox events.

#![allow(dead_code)] // Transport hook is wired in the next Phase 2 block.

use async_trait::async_trait;
use daruma_core::embed::EventEnvelope;
use daruma_shared::Result;

use crate::outbox::Outbox;

/// What the server did with one pushed event. A transient failure
/// (network, 5xx, 401/408/429) is an `Err`: the flush stops and retries later.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PushOutcome {
    Accepted,
    /// Refused for good (a final 4xx): retrying the same event cannot help.
    Rejected {
        status: u16,
        code: String,
    },
}

#[async_trait]
pub trait RemoteEventSink: Send + Sync {
    async fn push(&self, envelope: EventEnvelope) -> Result<PushOutcome>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlushStats {
    pub attempted: usize,
    pub flushed: usize,
    /// Events dead-lettered in this flush.
    pub rejected: usize,
}

pub async fn flush_pending(
    outbox: &Outbox,
    sink: &dyn RemoteEventSink,
    limit: u32,
) -> Result<FlushStats> {
    let pending = outbox.pending(limit).await?;
    let (mut flushed, mut rejected) = (0usize, 0usize);
    for entry in &pending {
        match sink.push(entry.envelope.clone()).await? {
            PushOutcome::Accepted => {
                if outbox.mark_flushed(entry.id).await? {
                    flushed += 1;
                }
            }
            PushOutcome::Rejected { status, code } => {
                tracing::warn!(
                    origin_seq = entry.origin_seq,
                    status,
                    code = %code,
                    "server rejected outbox event; moved to dead letters"
                );
                outbox.mark_rejected(entry.id, status, &code).await?;
                rejected += 1;
            }
        }
    }
    Ok(FlushStats {
        attempted: pending.len(),
        flushed,
        rejected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_core::embed::{Db, Event, EventEnvelope};
    use daruma_domain::{Actor, NewTask};
    use daruma_shared::{CoreError, DeviceId};
    use std::sync::{Arc, Mutex};

    struct RecordingSink {
        seen: Arc<Mutex<Vec<u64>>>,
        fail_on_seq: Option<u64>,
        reject_seq: Option<u64>,
    }

    #[async_trait]
    impl RemoteEventSink for RecordingSink {
        async fn push(&self, envelope: EventEnvelope) -> Result<PushOutcome> {
            if Some(envelope.origin_seq) == self.fail_on_seq {
                return Err(CoreError::sync("remote unavailable"));
            }
            if Some(envelope.origin_seq) == self.reject_seq {
                return Ok(PushOutcome::Rejected {
                    status: 403,
                    code: "intake_source_admin_only".into(),
                });
            }
            self.seen.lock().unwrap().push(envelope.origin_seq);
            Ok(PushOutcome::Accepted)
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
            reject_seq: None,
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
            reject_seq: None,
        };

        assert!(flush_pending(&outbox, &sink, 100).await.is_err());
        assert_eq!(outbox.pending(100).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rejected_event_is_dead_lettered_and_queue_moves_on() {
        let db = Db::memory().await.unwrap();
        let outbox = Outbox::new(db);
        outbox.ensure_schema().await.unwrap();
        let device = DeviceId::new();
        for (seq, title) in [(1, "one"), (2, "two"), (3, "three")] {
            outbox.enqueue(device, seq, envelope(title)).await.unwrap();
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = RecordingSink {
            seen: seen.clone(),
            fail_on_seq: None,
            reject_seq: Some(2),
        };
        let stats = flush_pending(&outbox, &sink, 100).await.unwrap();

        assert_eq!((stats.flushed, stats.rejected), (2, 1));
        assert_eq!(*seen.lock().unwrap(), vec![1, 3]);
        assert!(outbox.pending(100).await.unwrap().is_empty());
        assert_eq!(outbox.rejected_count().await.unwrap(), 1);
    }
}
