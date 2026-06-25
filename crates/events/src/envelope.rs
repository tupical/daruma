use serde::{Deserialize, Serialize};
use daruma_domain::Actor;
use daruma_shared::{time, DeviceId, EventId, Timestamp};

use crate::event::Event;

/// An immutable, append-only entry in the event log.
///
/// `seq` is `0` until the store assigns a monotonic value on append.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub id: EventId,
    #[serde(default)]
    pub seq: u64,
    pub occurred_at: Timestamp,
    pub actor: Actor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_device_id: Option<DeviceId>,
    #[serde(default)]
    pub origin_seq: u64,
    pub payload: Event,
}

impl EventEnvelope {
    /// Build a fresh envelope (seq=0) ready to hand to an [`EventStore`].
    pub fn new(actor: Actor, payload: Event) -> Self {
        Self {
            id: EventId::new(),
            seq: 0,
            occurred_at: time::now(),
            actor,
            origin_device_id: None,
            origin_seq: 0,
            payload,
        }
    }

    #[inline]
    pub fn kind(&self) -> &'static str {
        self.payload.kind()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_domain::NewTask;

    #[test]
    fn legacy_json_without_origin_fields_deserialises() {
        let env = EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated {
                task: NewTask::new("legacy"),
            },
        );
        let mut json = serde_json::to_value(&env).unwrap();
        let obj = json.as_object_mut().unwrap();
        obj.remove("origin_device_id");
        obj.remove("origin_seq");

        let back: EventEnvelope = serde_json::from_value(json).unwrap();
        assert_eq!(back.origin_device_id, None);
        assert_eq!(back.origin_seq, 0);
    }

    #[test]
    fn new_envelope_defaults_to_local_origin() {
        let env = EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated {
                task: NewTask::new("local"),
            },
        );
        assert_eq!(env.origin_device_id, None);
        assert_eq!(env.origin_seq, 0);
    }
}
