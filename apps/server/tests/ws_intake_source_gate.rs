//! A WS `dispatch` frame must not write a project's `intake_source` policy
//! (ADR-0009): only PATCH /v1/projects/{id}/settings may. `auto_append`
//! still goes through WS.

use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tower::ServiceExt;

mod common;
use common::{json_get, json_post, spawn_server, test_app};

/// The next `ack` / `error` frame.
async fn next<S, E>(stream: &mut S) -> Value
where
    S: futures::Stream<Item = Result<Message, E>> + Unpin,
    E: std::fmt::Debug,
{
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("WS frame in time")
            .expect("WS open")
            .expect("WS frame");
        let Message::Text(text) = msg else { continue };
        let frame: Value = serde_json::from_str(&text).unwrap();
        if matches!(frame["type"].as_str(), Some("ack" | "error")) {
            return frame;
        }
    }
}

#[tokio::test]
async fn ws_dispatch_cannot_write_intake_source() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let (_, body) = json_post(
        app.router.clone(),
        &app.admin_token,
        "/v1/commands",
        &json!({ "command": { "type": "create_project", "title": "Gate" }, "actor": { "kind": "user" } })
            .to_string(),
    )
    .await;
    let project_id = body["data"][0]["payload"]["project"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let settings_uri = format!("/v1/projects/{project_id}/settings");
    let req = Request::builder()
        .method(Method::PATCH)
        .uri(&settings_uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", app.admin_token))
        .body(Body::from(
            json!({ "intake_source": { "mode": "enforce" } }).to_string(),
        ))
        .unwrap();
    let res = app.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );

    let (stream, _) = connect_async(format!("ws://{addr}/v1/ws?token={}", app.admin_token))
        .await
        .expect("WS connect");
    let (mut sink, mut stream) = stream.split();
    for intake_source in [json!(null), json!({ "mode": "off" })] {
        let frame = json!({
            "type": "dispatch",
            "command": {
                "type": "update_project_settings",
                "project_id": project_id,
                "intake_source": intake_source,
            }
        });
        sink.send(Message::Text(frame.to_string().into()))
            .await
            .unwrap();
        let reply = next(&mut stream).await;
        assert_eq!(reply["type"], "error", "{reply}");
        assert_eq!(reply["code"], "intake_source_http_only", "{reply}");
        assert_eq!(
            reply["message"],
            "intake_source can be changed only via PATCH /v1/projects/{id}/settings"
        );
    }

    let frame = json!({
        "type": "dispatch",
        "command": {
            "type": "update_project_settings",
            "project_id": project_id,
            "auto_append": { "interview": false },
        }
    });
    sink.send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
    let reply = next(&mut stream).await;
    assert_eq!(reply["type"], "ack", "{reply}");

    let (status, body) = json_get(app.router.clone(), &app.admin_token, &settings_uri).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["intake_source"]["mode"], "enforce",
        "policy unchanged: {body}"
    );
    assert_eq!(body["auto_append"]["interview"], false, "{body}");
}
