//! WorkspaceGraph sidecar wiring — catch-up replay and live event subscription.

use std::sync::Arc;

use taskagent_events::{EventBus, EventStore};
use taskagent_shared::Result;
use taskagent_storage::WorkspaceGraphRepo;
use tokio::sync::broadcast::error::RecvError;

pub const CATCHUP_BATCH_SIZE: usize = 500;

/// Replay events from the canonical store into the sidecar index.
///
/// Starts from `status.last_event_seq` (or 0 when absent) and applies every
/// subsequent envelope in batches of [`CATCHUP_BATCH_SIZE`].
pub async fn catch_up_from_events(
    graph: &WorkspaceGraphRepo,
    store: &dyn EventStore,
) -> Result<u64> {
    let status = graph.status().await?;
    let mut last = status.last_event_seq.unwrap_or(0);
    let mut count = 0u64;
    loop {
        let batch = store.load_since(last, CATCHUP_BATCH_SIZE).await?;
        if batch.is_empty() {
            break;
        }
        for env in &batch {
            if let Err(e) = graph.apply_event(env).await {
                tracing::warn!(
                    err = %e,
                    seq = env.seq,
                    "workspace graph apply_event failed during catch-up"
                );
            } else {
                count += 1;
            }
        }
        // SAFETY: batch is non-empty; unwrap cannot panic.
        last = batch.last().unwrap().seq;
    }
    Ok(count)
}

/// Subscribe to the in-process event bus and incrementally update the graph.
pub fn spawn_subscriber(graph: Arc<WorkspaceGraphRepo>, bus: EventBus) {
    tokio::spawn(async move {
        let mut rx = bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(env) => {
                    if let Err(e) = graph.apply_event(&env).await {
                        tracing::warn!(
                            err = %e,
                            seq = env.seq,
                            "workspace graph apply_event failed"
                        );
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "workspace graph subscriber lagged");
                }
                Err(RecvError::Closed) => {
                    tracing::info!("workspace graph subscriber: bus closed, exiting");
                    return;
                }
            }
        }
    });
}
