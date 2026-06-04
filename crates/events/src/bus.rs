use tokio::sync::broadcast;

use crate::envelope::EventEnvelope;

pub type EventSender = broadcast::Sender<EventEnvelope>;
pub type EventReceiver = broadcast::Receiver<EventEnvelope>;

/// In-process broadcast of events.
///
/// Late subscribers only see events published after they subscribe.
/// Use [`EventStore::load_since`] for replay of historical events.
#[derive(Clone, Debug)]
pub struct EventBus {
    tx: EventSender,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity.max(1));
        Self { tx }
    }

    /// Send an event to every current subscriber.
    /// Returns the number of receivers it reached (0 if none).
    pub fn publish(&self, envelope: EventEnvelope) -> usize {
        self.tx.send(envelope).unwrap_or(0)
    }

    pub fn subscribe(&self) -> EventReceiver {
        self.tx.subscribe()
    }

    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }

    pub fn sender(&self) -> EventSender {
        self.tx.clone()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}
