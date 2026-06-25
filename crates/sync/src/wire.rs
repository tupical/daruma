//! Wire types for the WebSocket protocol.
//!
//! Types are now defined in `crates/api-dto` (wasm-compatible) and
//! re-exported here for backward compatibility.

pub use daruma_api_dto::ws::{WsClientMessage, WsServerMessage};

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_api_dto::command::Command;
    use daruma_domain::{Actor, NewTask};
    use daruma_events::{Channel, Event, EventEnvelope};
    use daruma_shared::ProjectId;

    #[test]
    fn dispatch_roundtrip() {
        let client_event_id = daruma_shared::EventId::new();
        let msg = WsClientMessage::Dispatch {
            command: Command::CreateTask {
                task: NewTask::new("wire test"),
            },
            actor: Some(Actor::user()),
            client_event_id: Some(client_event_id),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"dispatch\""), "json={json}");
        let back: WsClientMessage = serde_json::from_str(&json).unwrap();
        match back {
            WsClientMessage::Dispatch {
                client_event_id: Some(back_id),
                ..
            } => assert_eq!(back_id, client_event_id),
            _ => panic!("expected Dispatch with client_event_id"),
        }
    }

    #[test]
    fn legacy_dispatch_without_client_event_id_parses() {
        let json =
            r#"{"type":"dispatch","command":{"type":"create_task","task":{"title":"legacy"}}}"#;
        let back: WsClientMessage = serde_json::from_str(json).unwrap();
        match back {
            WsClientMessage::Dispatch {
                client_event_id, ..
            } => assert!(client_event_id.is_none()),
            _ => panic!("expected Dispatch"),
        }
    }

    #[test]
    fn server_event_roundtrip() {
        let env = EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated {
                task: NewTask::new("event wire"),
            },
        );
        let id = env.id;
        let msg = WsServerMessage::Event { envelope: env };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"event\""), "json={json}");
        let back: WsServerMessage = serde_json::from_str(&json).unwrap();
        match back {
            WsServerMessage::Event { envelope } => assert_eq!(envelope.id, id),
            _ => panic!("expected WsServerMessage::Event"),
        }
    }

    #[test]
    fn snapshot_roundtrip() {
        let env = EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated {
                task: NewTask::new("snapshot wire"),
            },
        );
        let id = env.id;
        let msg = WsServerMessage::Snapshot {
            since_seq: 42,
            events: vec![env],
            has_more: false,
            next_seq: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"snapshot\""), "json={json}");
        let back: WsServerMessage = serde_json::from_str(&json).unwrap();
        match back {
            WsServerMessage::Snapshot {
                since_seq, events, ..
            } => {
                assert_eq!(since_seq, 42);
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].id, id);
            }
            _ => panic!("expected WsServerMessage::Snapshot"),
        }
    }

    #[test]
    fn hello_resync_roundtrip() {
        let hello = WsServerMessage::Hello {
            server_seq: 7,
            capabilities: vec![
                "channels".to_string(),
                "resync".to_string(),
                "plans".to_string(),
                "runs".to_string(),
                "capability-gated-channels".to_string(),
            ],
        };
        let j = serde_json::to_string(&hello).unwrap();
        assert!(j.contains("\"type\":\"hello\""));
        let back: WsServerMessage = serde_json::from_str(&j).unwrap();
        assert!(matches!(back, WsServerMessage::Hello { server_seq: 7, .. }));

        let resync = WsServerMessage::Resync {
            from_seq: 100,
            dropped: 5,
        };
        let j = serde_json::to_string(&resync).unwrap();
        assert!(j.contains("\"type\":\"resync\""));
        let back: WsServerMessage = serde_json::from_str(&j).unwrap();
        assert!(matches!(
            back,
            WsServerMessage::Resync {
                from_seq: 100,
                dropped: 5
            }
        ));
    }

    #[test]
    fn subscribe_with_filters_roundtrip() {
        let msg = WsClientMessage::Subscribe {
            since_seq: Some(5),
            projects: Some(vec![ProjectId::new()]),
            channels: Some(vec![Channel::Tasks, Channel::Comments]),
            assignee: None,
            verb: None,
            parent_plan: None,
        };
        let j = serde_json::to_string(&msg).unwrap();
        let back: WsClientMessage = serde_json::from_str(&j).unwrap();
        assert!(matches!(
            back,
            WsClientMessage::Subscribe {
                since_seq: Some(5),
                projects: Some(_),
                channels: Some(_),
                ..
            }
        ));
    }

    /// §3.7.6 — `assignee`/`verb`/`parent_plan` round-trip and stay
    /// backward-compatible (omitted fields parse as `None`).
    #[test]
    fn subscribe_b6_filters_roundtrip() {
        let msg = WsClientMessage::Subscribe {
            since_seq: None,
            projects: None,
            channels: None,
            assignee: Some("agt_00000000-0000-0000-0000-000000000001".to_string()),
            verb: Some("task_status_changed".to_string()),
            parent_plan: Some("pln_00000000-0000-0000-0000-000000000002".to_string()),
        };
        let j = serde_json::to_string(&msg).unwrap();
        assert!(j.contains("\"assignee\""));
        assert!(j.contains("\"verb\""));
        assert!(j.contains("\"parent_plan\""));
        let back: WsClientMessage = serde_json::from_str(&j).unwrap();
        match back {
            WsClientMessage::Subscribe {
                assignee,
                verb,
                parent_plan,
                ..
            } => {
                assert_eq!(verb.as_deref(), Some("task_status_changed"));
                assert!(assignee.is_some());
                assert!(parent_plan.is_some());
            }
            _ => panic!("expected Subscribe"),
        }

        // Backward-compat: legacy client without the new fields parses fine.
        let legacy = r#"{"type":"subscribe","since_seq":7}"#;
        let parsed: WsClientMessage = serde_json::from_str(legacy).unwrap();
        match parsed {
            WsClientMessage::Subscribe {
                since_seq,
                assignee,
                verb,
                parent_plan,
                ..
            } => {
                assert_eq!(since_seq, Some(7));
                assert!(assignee.is_none());
                assert!(verb.is_none());
                assert!(parent_plan.is_none());
            }
            _ => panic!("expected Subscribe"),
        }
    }
}
