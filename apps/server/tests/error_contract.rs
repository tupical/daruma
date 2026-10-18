//! Pins the wire facts `docs/MODULE_CONTRACT.md` states about errors and
//! envelopes, so the contract cannot drift from the code again:
//!
//!   * handler errors (`ApiError`) are JSON `{"error":{"code","message"}}`;
//!   * framework-level rejections (missing `Content-Type`, malformed or
//!     schema-mismatched JSON body, unknown route, wrong method) are NOT
//!     JSON — the contract documents them as exceptions;
//!   * `EventEnvelope` serialises its identifier as `id`, not `event_id`
//!     (`event_id` is the `MutationResponse` field that carries the last
//!     envelope's `id`).

use axum::{
    body::{to_bytes, Body},
    http::{header::CONTENT_TYPE, Method, Request, StatusCode},
};
use serde_json::Value;
use tower::ServiceExt;

mod common;
use common::test_app;

struct Raw {
    status: StatusCode,
    content_type: Option<String>,
    body: Vec<u8>,
}

async fn send(app: &axum::Router, req: Request<Body>) -> Raw {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let content_type = res
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Raw {
        status,
        content_type,
        body,
    }
}

fn commands_request(token: &str, content_type: Option<&str>, body: &str) -> Request<Body> {
    let mut b = Request::builder()
        .method(Method::POST)
        .uri("/v1/commands")
        .header("authorization", format!("Bearer {token}"));
    if let Some(ct) = content_type {
        b = b.header(CONTENT_TYPE, ct);
    }
    b.body(Body::from(body.to_owned())).unwrap()
}

#[tokio::test]
async fn handler_error_is_json_error_object() {
    let h = test_app().await;
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/v1/tasks/{}", uuid::Uuid::now_v7()))
        .header("authorization", format!("Bearer {}", h.admin_token))
        .body(Body::empty())
        .unwrap();
    let raw = send(&h.router, req).await;

    assert_eq!(raw.status, StatusCode::NOT_FOUND);
    assert_eq!(raw.content_type.as_deref(), Some("application/json"));
    let json: Value = serde_json::from_slice(&raw.body).expect("JSON error body");
    assert_eq!(json["error"]["code"], "not_found", "{json}");
    assert!(json["error"]["message"].is_string(), "{json}");
}

#[tokio::test]
async fn missing_content_type_is_framework_text_rejection() {
    let h = test_app().await;
    let raw = send(
        &h.router,
        commands_request(
            &h.admin_token,
            None,
            r#"{"command":{"type":"create_project","title":"x"}}"#,
        ),
    )
    .await;

    assert_eq!(raw.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(
        raw.content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with("text/plain")),
        "content-type: {:?}",
        raw.content_type
    );
    assert!(serde_json::from_slice::<Value>(&raw.body).is_err());
}

#[tokio::test]
async fn malformed_json_body_is_framework_text_rejection() {
    let h = test_app().await;
    let raw = send(
        &h.router,
        commands_request(&h.admin_token, Some("application/json"), "{not json"),
    )
    .await;

    assert_eq!(raw.status, StatusCode::BAD_REQUEST);
    assert!(
        raw.content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with("text/plain")),
        "content-type: {:?}",
        raw.content_type
    );
    assert!(serde_json::from_slice::<Value>(&raw.body).is_err());
}

#[tokio::test]
async fn unknown_route_is_bare_404() {
    let h = test_app().await;
    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/no-such-route")
        .header("authorization", format!("Bearer {}", h.admin_token))
        .body(Body::empty())
        .unwrap();
    let raw = send(&h.router, req).await;

    assert_eq!(raw.status, StatusCode::NOT_FOUND);
    assert!(raw.body.is_empty(), "body: {:?}", raw.body);
}

#[tokio::test]
async fn event_envelope_serialises_identifier_as_id() {
    let h = test_app().await;
    let raw = send(
        &h.router,
        commands_request(
            &h.admin_token,
            Some("application/json"),
            r#"{"command":{"type":"create_project","title":"Envelope id check"}}"#,
        ),
    )
    .await;
    assert_eq!(raw.status, StatusCode::OK);
    let resp: Value = serde_json::from_slice(&raw.body).unwrap();

    let envelopes = resp["data"].as_array().expect("data array");
    for envelope in envelopes {
        assert!(envelope["id"].is_string(), "{envelope}");
        assert!(envelope.get("event_id").is_none(), "{envelope}");
    }
    // `MutationResponse.event_id` carries the last envelope's `id`.
    let last = envelopes.last().expect("at least one envelope");
    assert_eq!(resp["event_id"], last["id"], "{resp}");
}

#[tokio::test]
async fn schema_mismatch_body_is_framework_text_rejection() {
    let h = test_app().await;
    let raw = send(
        &h.router,
        commands_request(&h.admin_token, Some("application/json"), r#"{"foo":1}"#),
    )
    .await;

    assert_eq!(raw.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        raw.content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with("text/plain")),
        "content-type: {:?}",
        raw.content_type
    );
    assert!(serde_json::from_slice::<Value>(&raw.body).is_err());
}

#[tokio::test]
async fn wrong_method_is_bare_405() {
    let h = test_app().await;
    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/commands")
        .header("authorization", format!("Bearer {}", h.admin_token))
        .body(Body::empty())
        .unwrap();
    let raw = send(&h.router, req).await;

    assert_eq!(raw.status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(raw.body.is_empty(), "body: {:?}", raw.body);
}
