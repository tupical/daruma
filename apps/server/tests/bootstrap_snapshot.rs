//! `GET /v1/events/snapshot`: only a snapshot that holds every event up to
//! its seq reaches a device.

mod common;

use axum::http::StatusCode;
use common::{json_get, test_app};
use daruma_domain::{Actor, NewTask};
use daruma_events::{Event, EventEnvelope};
use serde_json::{json, Value};

#[tokio::test]
async fn legacy_rows_answer_null_until_a_consistent_snapshot_exists() {
    let app = test_app().await;
    let token = app.admin_token.clone();
    let event = app
        .event_store()
        .append(EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated {
                task: NewTask::new("in the log only"),
            },
        ))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO snapshots (id, seq, created_at, payload_json) \
         VALUES ('legacy', ?, '2026-01-01T00:00:00+00:00', '{\"tasks\":[],\"projects\":[],\"comments\":[]}')",
    )
    .bind(event.seq as i64)
    .execute(&app.pool)
    .await
    .unwrap();

    let (status, body) = json_get(app.router.clone(), &token, "/v1/events/snapshot").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::Null);

    let seq = app
        .state
        .snapshots
        .write_if_due(&*app.event_store(), 1)
        .await
        .unwrap();
    assert_eq!(seq, Some(event.seq));

    let (status, body) = json_get(app.router.clone(), &token, "/v1/events/snapshot").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["seq"], json!(event.seq));
    assert_eq!(body["cut_version"], json!(1));
    assert_eq!(body["payload"]["tasks"][0]["title"], "in the log only");
}
