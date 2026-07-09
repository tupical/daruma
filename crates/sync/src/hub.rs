//! `Hub` — bridges the in-process [`EventBus`] to WebSocket fanout and
//! exposes a command entry-point for WS transports.
//!
//! # WS fanout architecture (§3.9.4)
//!
//! Direct `tokio::broadcast::Receiver` fanout to N WebSocket subscribers
//! suffers from the classic slow-receiver problem: one slow client
//! overflows the shared ring buffer, every receiver returns
//! `RecvError::Lagged`, and the server emits `Resync` to the entire herd.
//!
//! Hub adds an internal fanout layer:
//!
//! 1. `Hub::new` subscribes **once** to the broadcast bus and spawns a
//!    long-lived `fanout_task` that does only synchronous work between
//!    `recv().await` calls (so it never lags broadcast itself).
//! 2. `Hub::subscribe_ws()` registers a per-subscriber
//!    `mpsc::Sender<Arc<EventEnvelope>>` in a [`DashMap`] and returns the
//!    paired [`WsSubscription`] RAII guard (Drop removes the entry).
//! 3. The fanout task `try_send`s `Arc::clone(&envelope)` to every entry.
//!    On [`TrySendError::Full`] the subscriber is dropped immediately:
//!    its sender goes out of scope, its writer task observes `None` on
//!    `recv()`, the socket closes, and the client reconnects with
//!    `since_seq` and rehydrates via the existing snapshot path.
//!
//! The broadcast bus is preserved for non-WS consumers (webhook
//! dispatcher, agent-inbox long-poll). Only the WS pathway is rewired.

use std::sync::Arc;

use daruma_core::{Command, CommandBus};
use daruma_domain::Actor;
use daruma_events::{EventBus, EventEnvelope, EventReceiver};
use daruma_shared::{DeviceId, Result};
use dashmap::DashMap;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use uuid::Uuid;

/// Per-subscriber WS fanout channel capacity.
///
/// Bound chosen from research compass artifact §A.1; closes one slow consumer
/// without affecting others.
pub const WS_SUBSCRIBER_CHANNEL: usize = 64;

/// Stable identifier for a WS subscription registration. Used as the
/// [`DashMap`] key for the fanout map.
pub type SubscriptionId = Uuid;

/// Sender half of a per-subscriber WS frame channel. Carries
/// `Arc<EventEnvelope>` so the fanout task only pays an atomic bump per
/// subscriber on each event.
pub type FrameSender = mpsc::Sender<Arc<EventEnvelope>>;

/// Receiver half handed back to the WS connection's writer task.
pub type FrameReceiver = mpsc::Receiver<Arc<EventEnvelope>>;

type SubscriberMap = DashMap<SubscriptionId, FrameSender>;

/// Central coordination point for the sync layer.
///
/// `Hub` is cheap to clone — all state is behind `Arc`. The fanout task
/// is started exactly once, on construction, and lives until the last
/// `Hub` clone is dropped.
#[derive(Clone)]
pub struct Hub {
    pub bus: EventBus,
    pub commands: Arc<CommandBus>,
    inner: Arc<HubInner>,
}

struct HubInner {
    subscribers: Arc<SubscriberMap>,
    connected_devices: Arc<DashMap<DeviceId, usize>>,
    device_revocations: broadcast::Sender<DeviceId>,
    fanout_task: AbortHandle,
}

impl Drop for HubInner {
    fn drop(&mut self) {
        // Belt-and-braces shutdown. The fanout task also exits naturally
        // on `RecvError::Closed` once the last `EventBus` clone is gone,
        // but aborting here makes Hub drop deterministic and avoids
        // dangling tasks in tests that build/drop a `Hub` rapidly.
        self.fanout_task.abort();
    }
}

impl Hub {
    pub fn new(bus: EventBus, commands: Arc<CommandBus>) -> Self {
        let subscribers: Arc<SubscriberMap> = Arc::new(DashMap::new());
        let connected_devices = Arc::new(DashMap::new());
        let (device_revocations, _rx) = broadcast::channel(128);
        let bus_rx = bus.subscribe();
        let fanout_handle = spawn_fanout(bus_rx, Arc::clone(&subscribers));

        Self {
            bus,
            commands,
            inner: Arc::new(HubInner {
                subscribers,
                connected_devices,
                device_revocations,
                fanout_task: fanout_handle.abort_handle(),
            }),
        }
    }

    /// Subscribe to the live broadcast event stream.
    ///
    /// Kept for non-WS consumers (webhook dispatcher, agent-inbox
    /// long-poll). WS connections must use [`subscribe_ws`](Self::subscribe_ws)
    /// instead — that path gets per-subscriber backpressure.
    ///
    /// Late subscribers only see events published after this call.
    /// Use `EventStore::load_since` for historical replay.
    pub fn subscribe(&self) -> EventReceiver {
        self.bus.subscribe()
    }

    /// Register a fresh WS subscriber with the Hub fanout layer.
    ///
    /// Returns a [`WsSubscription`] that:
    /// - exposes the [`FrameReceiver`] via [`take_receiver`](WsSubscription::take_receiver),
    /// - **unregisters the subscriber on `Drop`** (RAII guard — move into
    ///   the spawned writer task so leaks are impossible).
    pub fn subscribe_ws(&self) -> WsSubscription {
        let id = Uuid::new_v4();
        let (tx, rx) = mpsc::channel(WS_SUBSCRIBER_CHANNEL);
        self.inner.subscribers.insert(id, tx);
        WsSubscription {
            id,
            rx: Some(rx),
            subscribers: Arc::clone(&self.inner.subscribers),
        }
    }

    /// Dispatch a command through the [`CommandBus`] and return the
    /// persisted envelopes.
    pub async fn handle_command(&self, cmd: Command, actor: Actor) -> Result<Vec<EventEnvelope>> {
        self.commands.dispatch(cmd, actor).await
    }

    pub fn device_connected(&self, id: DeviceId) {
        self.inner
            .connected_devices
            .entry(id)
            .and_modify(|count| *count += 1)
            .or_insert(1);
    }

    pub fn device_disconnected(&self, id: DeviceId) {
        if let Some(mut count) = self.inner.connected_devices.get_mut(&id) {
            if *count > 1 {
                *count -= 1;
                return;
            }
        }
        self.inner.connected_devices.remove(&id);
    }

    pub fn connected_devices(&self) -> Vec<DeviceId> {
        self.inner
            .connected_devices
            .iter()
            .map(|entry| *entry.key())
            .collect()
    }

    pub fn notify_device_revoked(&self, id: DeviceId) {
        let _ = self.inner.device_revocations.send(id);
    }

    pub fn subscribe_device_revocations(&self) -> broadcast::Receiver<DeviceId> {
        self.inner.device_revocations.subscribe()
    }

    /// Current number of registered WS subscribers. Test helper.
    #[doc(hidden)]
    pub fn ws_subscriber_count(&self) -> usize {
        self.inner.subscribers.len()
    }
}

/// RAII handle for a single WS subscription.
///
/// Dropping the guard removes the sender from the fanout map. Hold this
/// alive for the lifetime of the WS connection (typically by moving it
/// into the writer task).
pub struct WsSubscription {
    pub id: SubscriptionId,
    rx: Option<FrameReceiver>,
    subscribers: Arc<SubscriberMap>,
}

impl WsSubscription {
    /// Take ownership of the [`FrameReceiver`]. Panics if called twice
    /// on the same subscription.
    pub fn take_receiver(&mut self) -> FrameReceiver {
        self.rx
            .take()
            .expect("WsSubscription receiver already taken")
    }
}

impl Drop for WsSubscription {
    fn drop(&mut self) {
        self.subscribers.remove(&self.id);
    }
}

/// Long-lived fanout task: drain broadcast bus → try_send to every
/// per-subscriber mpsc, reaping full/closed senders in place.
fn spawn_fanout(
    mut bus_rx: EventReceiver,
    subscribers: Arc<SubscriberMap>,
) -> tokio::task::JoinHandle<()> {
    // TODO(§3.9.x follow-up): cheap pre-filter at fanout (Channel / inline
    // project_id) so highly-filtered subscribers don't waste 64-slot buffers.
    // Must stay sync-only — no DB lookups in fanout hot loop.
    tokio::spawn(async move {
        loop {
            match bus_rx.recv().await {
                Ok(env) => {
                    let arc = Arc::new(env);
                    subscribers.retain(|_id, tx| match tx.try_send(Arc::clone(&arc)) {
                        Ok(()) => true,
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            // Slow subscriber → drop sender → writer task
                            // observes `None` → socket closes → client
                            // reconnects with `since_seq` and rehydrates.
                            tracing::info!(
                                subscription_id = %_id,
                                "ws subscriber dropped: mpsc full"
                            );
                            false
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            // Receiver already dropped (handler exited).
                            // Reap the stale entry.
                            false
                        }
                    });
                }
                Err(RecvError::Lagged(skipped)) => {
                    // Fanout task itself fell behind broadcast. Its body is
                    // pure-sync `try_send` so this should be rare (only under
                    // pathological load), but we must NOT silently lose
                    // events: the ring buffer overwrote some envelopes, we
                    // cannot know which, and subscribers downstream would
                    // otherwise miss them with no recovery signal.
                    //
                    // Response: drop every registered subscriber. Each
                    // writer task sees its mpsc closed → socket closes →
                    // client reconnects with `since_seq` and rehydrates via
                    // the existing snapshot path. Same recovery path as the
                    // per-subscriber slow-drop, just triggered globally.
                    tracing::error!(
                        skipped,
                        "ws hub fanout lagged broadcast — closing all subscribers for cursor-rehydrate"
                    );
                    subscribers.clear();
                }
                Err(RecvError::Closed) => {
                    // EventBus dropped. Nothing more to do.
                    break;
                }
            }
        }
    })
}

// ─── tests ────────────────────────────────────────────────────────────────────
//
// The fanout layer is unit-tested in isolation: `spawn_fanout` takes only an
// `EventReceiver` + the shared `Arc<SubscriberMap>` and is independent of the
// `CommandBus` wired through `Hub`. End-to-end coverage (Hub + CommandBus + WS
// stack) lives in `apps/server/tests/ws_slow_consumer.rs`.

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_domain::{Actor, NewTask};
    use daruma_events::{Event, EventBus, EventEnvelope};

    fn synthetic_envelope(seq: u64) -> EventEnvelope {
        let mut env = EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated {
                task: NewTask::new(format!("seq-{seq}")),
            },
        );
        env.seq = seq;
        env
    }

    /// Run the fanout test scaffolding: build EventBus, spawn the fanout task,
    /// return `(bus, subscribers, fanout_handle)`. Callers register subscribers
    /// via `subscribers.insert(...)` and abort the task with `fanout_handle`.
    fn spawn_fanout_under_test() -> (EventBus, Arc<SubscriberMap>, tokio::task::JoinHandle<()>) {
        let bus = EventBus::new(1024);
        let subscribers: Arc<SubscriberMap> = Arc::new(DashMap::new());
        let handle = spawn_fanout(bus.subscribe(), Arc::clone(&subscribers));
        (bus, subscribers, handle)
    }

    fn register_subscriber(subscribers: &Arc<SubscriberMap>) -> (SubscriptionId, FrameReceiver) {
        let id = Uuid::new_v4();
        let (tx, rx) = mpsc::channel(WS_SUBSCRIBER_CHANNEL);
        subscribers.insert(id, tx);
        (id, rx)
    }

    async fn yield_runtime() {
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fanout_to_two_subscribers_share_arc() {
        let (bus, subscribers, fanout) = spawn_fanout_under_test();
        let (_id_a, mut rx_a) = register_subscriber(&subscribers);
        let (_id_b, mut rx_b) = register_subscriber(&subscribers);

        bus.publish(synthetic_envelope(1));
        yield_runtime().await;

        let got_a = rx_a.try_recv().expect("A must receive");
        let got_b = rx_b.try_recv().expect("B must receive");

        // Same `Arc` → pointer-equal → no payload clone happened.
        assert!(
            Arc::ptr_eq(&got_a, &got_b),
            "subscribers must receive the same Arc<EventEnvelope> instance"
        );
        assert_eq!(got_a.seq, 1);

        fanout.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_subscriber_dropped_on_full() {
        let (bus, subscribers, fanout) = spawn_fanout_under_test();

        // Slow A: never drains.
        let (id_a, _rx_a) = register_subscriber(&subscribers);
        // Fast B: drains immediately.
        let (id_b, mut rx_b) = register_subscriber(&subscribers);
        assert_eq!(subscribers.len(), 2);

        // Publish WS_SUBSCRIBER_CHANNEL events. A's mpsc holds them all
        // (capacity = 64); no Full yet.
        for seq in 1..=WS_SUBSCRIBER_CHANNEL as u64 {
            bus.publish(synthetic_envelope(seq));
        }
        yield_runtime().await;

        // Drain B so the fanout task can keep up with B even after we
        // publish more. B receives every event because it always has slot
        // capacity.
        let mut received_b = 0u64;
        while rx_b.try_recv().is_ok() {
            received_b += 1;
        }
        assert_eq!(received_b, WS_SUBSCRIBER_CHANNEL as u64);

        // One more publish → A overflows and is reaped.
        bus.publish(synthetic_envelope(WS_SUBSCRIBER_CHANNEL as u64 + 1));
        yield_runtime().await;

        assert!(
            !subscribers.contains_key(&id_a),
            "slow subscriber A must be reaped after Full"
        );
        assert!(
            subscribers.contains_key(&id_b),
            "fast subscriber B must still be registered"
        );

        // B got the extra event too.
        let extra = rx_b.try_recv().expect("B must still receive");
        assert_eq!(extra.seq, WS_SUBSCRIBER_CHANNEL as u64 + 1);

        fanout.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drop_guard_unregisters() {
        // Directly exercise the Drop impl on `WsSubscription` against a
        // freshly-built `SubscriberMap`. No fanout task needed.
        let subscribers: Arc<SubscriberMap> = Arc::new(DashMap::new());
        let id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel(WS_SUBSCRIBER_CHANNEL);
        subscribers.insert(id, tx);
        assert_eq!(subscribers.len(), 1);

        let guard = WsSubscription {
            id,
            rx: None,
            subscribers: Arc::clone(&subscribers),
        };
        drop(guard);

        assert_eq!(subscribers.len(), 0, "Drop must unregister the subscriber");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn closed_receiver_is_reaped() {
        // Mirror Drop semantics: when the consumer drops its `Receiver`,
        // the next try_send returns `Closed` and the fanout reaps the entry.
        let (bus, subscribers, fanout) = spawn_fanout_under_test();
        let (id, rx) = register_subscriber(&subscribers);
        drop(rx); // simulate consumer that vanished without using the guard

        bus.publish(synthetic_envelope(1));
        yield_runtime().await;

        assert!(
            !subscribers.contains_key(&id),
            "subscriber with closed receiver must be reaped"
        );

        fanout.abort();
    }
}
