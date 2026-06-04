//! Integration tests for the public `/healthz` endpoint.
//!
//! §3.4 W2.2 — payload exposes `{status, version, core_version, api_version}`
//! so deploys/probes can detect drift between the running binary and
//! `taskagent-core`, and pick the right REST contract.

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::Value;
use tower::ServiceExt;

mod common;
use common::test_app;

async fn read_json(res: axum::http::Response<Body>) -> Value {
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn fetch_healthz(uri: &str) -> Value {
    let h = test_app().await;
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let res = h.router.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    read_json(res).await
}

#[tokio::test]
async fn root_healthz_returns_full_metadata() {
    let body = fetch_healthz("/healthz").await;

    assert_eq!(body["status"], "ok");
    assert_eq!(body["api_version"], "v1");

    let version = body["version"]
        .as_str()
        .expect("version should be a string");
    assert!(!version.is_empty(), "version must not be empty");

    let core_version = body["core_version"]
        .as_str()
        .expect("core_version should be a string");
    assert_eq!(core_version, taskagent_core::VERSION);
}

#[tokio::test]
async fn v1_healthz_matches_root() {
    let root = fetch_healthz("/healthz").await;
    let v1 = fetch_healthz("/v1/healthz").await;
    assert_eq!(root, v1);
}
