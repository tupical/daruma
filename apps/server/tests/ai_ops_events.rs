//! §3.8.12 — AI operation progress is pushed as typed events on
//! `Channel::AiOps` (started → phase → completed), not polled.

use taskagent_events::{Channel, Event};
use taskagent_shared::AiOpId;

mod common;
use common::test_app;

#[tokio::test]
async fn ai_op_events_flow_through_the_bus_on_aiops_channel() {
    let h = test_app().await;
    let handler = h.state.commands.handler();
    let mut rx = h.bus.subscribe();

    let op_id = AiOpId::new();
    let now = chrono::Utc::now();
    handler
        .emit_system_event(Event::AiOperationStarted {
            op_id,
            kind: "decompose".into(),
            target_id: "tsk_test".into(),
            at: now,
        })
        .await
        .unwrap();
    handler
        .emit_system_event(Event::AiOperationPhaseChanged {
            op_id,
            phase: "llm_call".into(),
            detail: None,
            at: now,
        })
        .await
        .unwrap();
    handler
        .emit_system_event(Event::AiOperationCompleted {
            op_id,
            outcome: "ok".into(),
            at: now,
        })
        .await
        .unwrap();

    let mut kinds = Vec::new();
    for _ in 0..3 {
        let env = rx.recv().await.expect("event on bus");
        assert_eq!(env.payload.channel(), Channel::AiOps);
        kinds.push(env.payload.kind());
        match &env.payload {
            Event::AiOperationStarted { op_id: got, .. }
            | Event::AiOperationPhaseChanged { op_id: got, .. }
            | Event::AiOperationCompleted { op_id: got, .. } => assert_eq!(*got, op_id),
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert_eq!(
        kinds,
        vec![
            "ai_operation_started",
            "ai_operation_phase_changed",
            "ai_operation_completed"
        ]
    );
}
