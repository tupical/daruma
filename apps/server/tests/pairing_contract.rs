//! Pairing contract (§3.3.5): the ticket advertises the fingerprint with a
//! single `sha256:` prefix and `POST /v1/devices/pair` requires a matching
//! `tls_fingerprint` from the client.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::{json_get, test_app, TestApp};

const FPR_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";

async fn issue_ticket(app: &TestApp) -> Value {
    let (status, ticket) = json_get(
        app.router.clone(),
        &app.admin_token,
        "/v1/devices/pair/ticket",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "ticket: {ticket}");
    ticket
}

async fn pair(app: &TestApp, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/devices/pair")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn url_param(url: &str, key: &str) -> String {
    let query = url.split_once('?').unwrap().1;
    let raw = query
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .unwrap();
    raw.replace("%3A", ":")
}

#[tokio::test]
async fn ticket_fingerprint_has_single_prefix_and_matches_url() {
    let app = test_app().await;
    let ticket = issue_ticket(&app).await;

    let expected = format!("sha256:{FPR_HEX}");
    assert_eq!(ticket["tls_fingerprint"], expected.as_str());
    assert_eq!(ticket["host"], "localhost:8443");
    let url = ticket["pairing_url"].as_str().unwrap();
    assert_eq!(url_param(url, "fpr"), expected);
    assert_eq!(url_param(url, "host"), "localhost:8443");
}

#[tokio::test]
async fn pair_with_ticket_fingerprint_issues_device_token() {
    let app = test_app().await;
    let ticket = issue_ticket(&app).await;
    let token = url_param(ticket["pairing_url"].as_str().unwrap(), "token");

    // Exactly what the desktop client sends: the advertised fingerprint.
    let (status, body) = pair(
        &app,
        json!({
            "token": token,
            "tls_fingerprint": ticket["tls_fingerprint"],
            "device_label": "contract-test",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "pair: {body}");
    assert!(body["access_token"].as_str().is_some_and(|t| !t.is_empty()));
    assert_eq!(body["server_url"], "https://localhost:8443");

    // Single-use: the same token cannot pair a second device.
    let (status, _) = pair(
        &app,
        json!({ "token": token, "tls_fingerprint": ticket["tls_fingerprint"] }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn pair_accepts_bare_or_uppercase_hex_fingerprint() {
    let app = test_app().await;
    let ticket = issue_ticket(&app).await;
    let token = url_param(ticket["pairing_url"].as_str().unwrap(), "token");

    let (status, body) = pair(
        &app,
        json!({ "token": token, "tls_fingerprint": FPR_HEX.to_ascii_uppercase() }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "pair: {body}");
}

#[tokio::test]
async fn pair_rejects_mismatched_double_prefixed_or_missing_fingerprint() {
    let app = test_app().await;

    let ticket = issue_ticket(&app).await;
    let token = url_param(ticket["pairing_url"].as_str().unwrap(), "token");
    let (status, _) = pair(
        &app,
        json!({ "token": token, "tls_fingerprint": format!("sha256:{}", "1".repeat(64)) }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let ticket = issue_ticket(&app).await;
    let token = url_param(ticket["pairing_url"].as_str().unwrap(), "token");
    let (status, _) = pair(
        &app,
        json!({ "token": token, "tls_fingerprint": format!("sha256:sha256:{FPR_HEX}") }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The pre-fix desktop body (no tls_fingerprint) is rejected up front.
    let ticket = issue_ticket(&app).await;
    let token = url_param(ticket["pairing_url"].as_str().unwrap(), "token");
    let (status, _) = pair(&app, json!({ "token": token })).await;
    assert!(status.is_client_error(), "missing fingerprint: {status}");
}
