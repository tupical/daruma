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
/// A projection error stops replay at the last successful envelope.
pub async fn catch_up_from_events(
    graph: &WorkspaceGraphRepo,
    store: &dyn EventStore,
) -> Result<u64> {
    replay(graph, store, true).await
}

async fn replay(
    graph: &WorkspaceGraphRepo,
    store: &dyn EventStore,
    reset_if_diverged: bool,
) -> Result<u64> {
    // Read the cursor before the store head: events are appended before they
    // are published, so a live subscriber can never push the cursor past it.
    let cursor = graph.cursor().await?;
    let mut last = cursor.as_ref().map_or(0, |c| c.seq);
    if let Some(reason) = rollback_reason(cursor.as_ref(), store).await? {
        if !reset_if_diverged {
            return Err(daruma_shared::CoreError::storage(format!(
                "workspace graph diverged from the journal: {reason}"
            )));
        }
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
                return Ok(count);
            }
            count += 1;
        }
        // SAFETY: batch is non-empty; unwrap cannot panic.
        last = batch.last().unwrap().seq;
    }
    Ok(count)
}

/// Replay a derived graph and fail if it still cannot serve current data.
pub async fn catch_up_current(graph: &WorkspaceGraphRepo, store: &dyn EventStore) -> Result<()> {
    graph.require_schema().await?;
    replay(graph, store, false).await?;
    require_current(graph, store).await
}

/// Verify a projection against its journal before using it for reads or decisions.
pub async fn require_current(graph: &WorkspaceGraphRepo, store: &dyn EventStore) -> Result<()> {
    graph.require_schema().await?;
    let head = store.latest_seq().await?;
    let cursor = graph.cursor().await?;
    let projected = cursor.as_ref().map_or(0, |cursor| cursor.seq);
    if projected < head || rollback_reason(cursor.as_ref(), store).await?.is_some() {
        return Err(daruma_shared::CoreError::storage(format!(
            "workspace graph cursor {projected} is not current with journal head {head}"
        )));
    }
    Ok(())
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

/// Resume replay from the saved cursor when the event bus signals new writes.
pub fn spawn_subscriber(graph: Arc<WorkspaceGraphRepo>, bus: EventBus, store: Arc<dyn EventStore>) {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(_) => {}
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "workspace graph subscriber lagged");
                }
                Err(RecvError::Closed) => {
                    tracing::info!("workspace graph subscriber: bus closed, exiting");
                    return;
                }
            }
            if let Err(e) = catch_up_from_events(&graph, store.as_ref()).await {
                tracing::warn!(err = %e, "workspace graph subscriber catch-up failed");
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
    async fn subscriber_recovers_backlog_after_startup_projection_error() {
        use tokio::time::{sleep, timeout, Duration};

        let (_live_db, live) = store().await;
        let live = Arc::new(live);
        for word in ["alpha", "beta", "gamma"] {
            live.append(task_event(word)).await.unwrap();
        }
        let (graph_db, graph_repo) = graph().await;
        let graph = Arc::new(graph_repo);
        sqlx::query(
            "CREATE TRIGGER reject_beta BEFORE INSERT ON workspacegraph_nodes
             WHEN NEW.title = 'beta' BEGIN SELECT RAISE(ABORT, 'test projection failure'); END",
        )
        .execute(graph_db.pool())
        .await
        .unwrap();
        assert_eq!(
            catch_up_from_events(&graph, live.as_ref()).await.unwrap(),
            1
        );
        assert_eq!(graph.cursor().await.unwrap().unwrap().seq, 1);
        sqlx::query("DROP TRIGGER reject_beta")
            .execute(graph_db.pool())
            .await
            .unwrap();

        let bus = EventBus::new(16);
        spawn_subscriber(graph.clone(), bus.clone(), live.clone());
        let delta = live.append(task_event("delta")).await.unwrap();
        assert_eq!(bus.publish(delta), 1);
        timeout(Duration::from_secs(5), async {
            while graph.cursor().await.unwrap().unwrap().seq < 4 {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("subscriber should project through sequence 4");

        assert_eq!(
            catch_up_from_events(&graph, live.as_ref()).await.unwrap(),
            0
        );
        assert_eq!(
            titles(&graph, &WORDS).await,
            ["alpha", "beta", "gamma", "delta"]
        );
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
