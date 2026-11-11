use daruma_domain::Actor;
use daruma_shared::{time, DeviceId, EventId, Timestamp};
use serde::{Deserialize, Serialize};

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
    /// Original non-secret selector, not a runtime/auth principal. Immutable
    /// provenance: alias and display-name changes never rewrite this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_agent_id: Option<String>,
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
            logical_agent_id: current_logical_agent(),
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

#[cfg(feature = "runtime")]
tokio::task_local! {
    static LOGICAL_AGENT: Option<String>;
}

/// Scope only authenticated, owner-validated request dispatch. Task locals do
/// not propagate to concurrent requests or newly spawned background tasks.
#[cfg(feature = "runtime")]
pub async fn scope_logical_agent<F: std::future::Future>(
    id: Option<String>,
    future: F,
) -> F::Output {
    LOGICAL_AGENT.scope(id, future).await
}

fn current_logical_agent() -> Option<String> {
    #[cfg(feature = "runtime")]
    {
        LOGICAL_AGENT.try_with(Clone::clone).ok().flatten()
    }
    #[cfg(not(feature = "runtime"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_domain::NewTask;

    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn request_attribution_is_isolated_and_not_inherited_by_system_tasks() {
        let make = || {
            EventEnvelope::new(
                Actor::user(),
                Event::TaskCreated {
                    task: NewTask::new("scope"),
                },
            )
        };
        let (a, b) = tokio::join!(
            scope_logical_agent(Some("a".into()), async {
                tokio::task::yield_now().await;
                make()
            }),
            scope_logical_agent(Some("b".into()), async {
                tokio::task::yield_now().await;
                make()
            }),
        );
        assert_eq!(a.logical_agent_id.as_deref(), Some("a"));
        assert_eq!(b.logical_agent_id.as_deref(), Some("b"));
        assert_eq!(make().logical_agent_id, None);
        let spawned = scope_logical_agent(Some("a".into()), async {
            tokio::spawn(async {
                EventEnvelope::new(
                    Actor::user(),
                    Event::TaskCreated {
                        task: NewTask::new("system"),
                    },
                )
            })
            .await
            .unwrap()
        })
        .await;
        assert_eq!(spawned.logical_agent_id, None);
        let mut legacy = serde_json::to_value(&a).unwrap();
        legacy.as_object_mut().unwrap().remove("logical_agent_id");
        let legacy: EventEnvelope = serde_json::from_value(legacy).unwrap();
        assert_eq!(legacy.logical_agent_id, None);
        assert_eq!(legacy.id, a.id);
        assert_eq!(legacy.actor, a.actor);
    }

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
