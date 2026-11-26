use std::sync::{Arc, OnceLock};

use tokio::sync::broadcast;

use crate::envelope::EventEnvelope;

pub type EventSender = broadcast::Sender<EventEnvelope>;
pub type EventReceiver = broadcast::Receiver<EventEnvelope>;

/// In-process broadcast of events.
///
/// Late subscribers only see events published after they subscribe.
/// Use [`EventStore::load_since`] for replay of historical events.
/// The ring is allocated when the first receiver or exported sender is needed.
#[derive(Clone, Debug)]
pub struct EventBus {
    tx: Arc<OnceLock<EventSender>>,
    capacity: usize,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        Self {
            tx: Arc::new(OnceLock::new()),
            capacity: capacity.max(1),
        }
    }

    fn channel(&self) -> &EventSender {
        self.tx.get_or_init(|| EventSender::new(self.capacity))
    }

    /// Send an event to every current subscriber.
    /// Returns the number of receivers it reached (0 if none).
    pub fn publish(&self, envelope: EventEnvelope) -> usize {
        self.tx.get().map_or(0, |tx| tx.send(envelope).unwrap_or(0))
    }

    pub fn subscribe(&self) -> EventReceiver {
        self.channel().subscribe()
    }

    pub fn receiver_count(&self) -> usize {
        self.tx.get().map_or(0, EventSender::receiver_count)
    }

    pub fn sender(&self) -> EventSender {
        self.channel().clone()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_domain::{Actor, NewTask};

    #[tokio::test]
    async fn clones_share_first_subscription_and_receive_only_future_events() {
        let bus = EventBus::new(4);
        let event = EventEnvelope::new(
            Actor::user(),
            crate::Event::TaskCreated {
                task: NewTask::new("lazy ring"),
            },
        );
        assert_eq!(bus.publish(event.clone()), 0);
        assert!(bus.tx.get().is_none());

        let clone = bus.clone();
        let mut receiver = clone.subscribe();
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(bus.sender().send(event.clone()).unwrap(), 1);
        assert_eq!(receiver.recv().await.unwrap().id, event.id);

        drop(clone);
        drop(bus);
        assert!(matches!(
            receiver.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));
    }
}
