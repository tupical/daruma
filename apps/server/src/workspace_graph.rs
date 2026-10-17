//! WorkspaceGraph sidecar wiring — catch-up replay and live event subscription.

use std::sync::Arc;

use daruma_events::{EventBus, EventStore};
use daruma_shared::Result;
use daruma_storage::{GraphCursor, WorkspaceGraphRepo};
use tokio::sync::broadcast::error::RecvError;

pub const CATCHUP_BATCH_SIZE: usize = 500;

/// Replay events from the canonical store into the sidecar index.
///
/// Starts from the graph's saved cursor (or 0 when absent) and applies every
/// subsequent envelope in batches of [`CATCHUP_BATCH_SIZE`]. When the cursor
/// no longer matches the store — the primary DB was restored from an older
/// backup — the index is reset and rebuilt from seq 0 instead, because
/// resuming would skip every event re-assigned to the rolled-back seqs.
pub async fn catch_up_from_events(
    graph: &WorkspaceGraphRepo,
    store: &dyn EventStore,
) -> Result<u64> {
    // Read the cursor before the store head: events are appended before they
    // are published, so a live subscriber can never push the cursor past it.
    let cursor = graph.cursor().await?;
    let mut last = cursor.as_ref().map_or(0, |c| c.seq);
    if let Some(reason) = rollback_reason(cursor.as_ref(), store).await? {
        tracing::warn!(
            graph_seq = last,
            reason,
            "workspace graph is ahead of or diverged from the event log; rebuilding"
        );
        graph.reset().await?;
        last = 0;
    }
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

/// Why the saved cursor cannot be resumed from, or `None` when it can.
async fn rollback_reason(
    cursor: Option<&GraphCursor>,
    store: &dyn EventStore,
) -> Result<Option<&'static str>> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    if cursor.seq == 0 {
        return Ok(None);
    }
    if cursor.seq > store.latest_seq().await? {
        return Ok(Some("cursor_ahead_of_log"));
    }
    let Some(saved_id) = cursor.event_id.as_deref() else {
        return Ok(None);
    };
    // Retention may have removed the event at the cursor; seqs are never
    // reused, so a missing row is not evidence of a rollback.
    let at_cursor = store.load_since(cursor.seq - 1, 1).await?;
    match at_cursor.first() {
        Some(env) if env.seq == cursor.seq && env.id.to_string() != saved_id => {
            Ok(Some("cursor_event_id_mismatch"))
        }
        _ => Ok(None),
    }
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

#[cfg(test)]
mod tests {
    use daruma_domain::{Actor, NewTask};
    use daruma_events::{Event, EventEnvelope};
    use daruma_shared::TaskId;
    use daruma_storage::{Db, SqliteEventStore};

    use super::*;

    async fn store() -> (Db, SqliteEventStore) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let store = SqliteEventStore::new(db.pool().clone());
        (db, store)
    }

    async fn graph() -> (Db, WorkspaceGraphRepo) {
        let db = Db::memory().await.unwrap();
        let graph = WorkspaceGraphRepo::new(db.pool().clone());
        graph.ensure_schema().await.unwrap();
        (db, graph)
    }

    fn task_event(title: &str) -> EventEnvelope {
        let mut task = NewTask::new(title);
        task.id = Some(TaskId::new());
        EventEnvelope::new(Actor::user(), Event::TaskCreated { task })
    }

    async fn titles(graph: &WorkspaceGraphRepo, words: &[&str]) -> Vec<String> {
        let mut found = Vec::new();
        for word in words {
            if !graph.search(word, 10, None).await.unwrap().is_empty() {
                found.push((*word).to_string());
            }
        }
        found
    }

    const WORDS: [&str; 6] = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"];

    /// Live log alpha, beta, gamma indexed to seq 3. Returns the graph and
    /// the alpha envelope, which an older backup shares with the live log.
    async fn indexed_live_log() -> (Db, WorkspaceGraphRepo, EventEnvelope) {
        let (_live_db, live) = store().await;
        let alpha = live.append(task_event("alpha")).await.unwrap();
        live.append(task_event("beta")).await.unwrap();
        live.append(task_event("gamma")).await.unwrap();
        let (graph_db, graph) = graph().await;
        assert_eq!(catch_up_from_events(&graph, &live).await.unwrap(), 3);
        assert_eq!(graph.cursor().await.unwrap().unwrap().seq, 3);
        (graph_db, graph, alpha)
    }

    #[tokio::test]
    async fn resume_without_rollback_applies_only_new_events() {
        let (_live_db, live) = store().await;
        live.append(task_event("alpha")).await.unwrap();
        let (_graph_db, graph) = graph().await;
        assert_eq!(catch_up_from_events(&graph, &live).await.unwrap(), 1);
        assert_eq!(catch_up_from_events(&graph, &live).await.unwrap(), 0);
        live.append(task_event("beta")).await.unwrap();
        assert_eq!(catch_up_from_events(&graph, &live).await.unwrap(), 1);
        assert_eq!(titles(&graph, &WORDS).await, ["alpha", "beta"]);
    }

    #[tokio::test]
    async fn restored_shorter_log_rebuilds_the_graph() {
        let (_graph_db, graph, alpha) = indexed_live_log().await;
        // Restore: backup taken after alpha, then one new write (seq 2).
        let (_backup_db, backup) = store().await;
        backup.append(alpha).await.unwrap();
        backup.append(task_event("delta")).await.unwrap();

        assert_eq!(catch_up_from_events(&graph, &backup).await.unwrap(), 2);
        assert_eq!(titles(&graph, &WORDS).await, ["alpha", "delta"]);
        assert_eq!(graph.cursor().await.unwrap().unwrap().seq, 2);
    }

    #[tokio::test]
    async fn restored_log_that_grew_past_the_cursor_rebuilds_the_graph() {
        let (_graph_db, graph, alpha) = indexed_live_log().await;
        // The restored log already has more events than the stale cursor, so
        // only the event id at seq 3 reveals the rollback.
        let (_backup_db, backup) = store().await;
        backup.append(alpha).await.unwrap();
        for word in ["delta", "epsilon", "zeta"] {
            backup.append(task_event(word)).await.unwrap();
        }

        assert_eq!(catch_up_from_events(&graph, &backup).await.unwrap(), 4);
        assert_eq!(
            titles(&graph, &WORDS).await,
            ["alpha", "delta", "epsilon", "zeta"]
        );
    }
}
