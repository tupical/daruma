//! Integration tests for the request-ID middleware.
//!
//! Builds a minimal in-process Axum router (just `/healthz`) with the
//! middleware applied and drives it with `tower::ServiceExt::oneshot`.

use axum::{body::Body, http::Request, http::StatusCode, routing::get, Router};
use daruma_server::middleware::request_id::request_id_middleware;
use tower::ServiceExt; // oneshot

fn test_router() -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn(request_id_middleware))
}

/// When no `X-Request-Id` is sent, the middleware generates one with the
/// `req_` prefix and adds it to the response.
#[tokio::test]
async fn request_id_generated_when_absent() {
    let app = test_router();

    let req = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();

    let res = app.oneshot(req).await.unwrap();

    assert_eq!(res.status(), StatusCode::OK);

    let id_header = res
        .headers()
        .get("x-request-id")
        .expect("response must contain X-Request-Id header");

    let id_str = id_header
        .to_str()
        .expect("header value must be valid UTF-8");
    assert!(
        id_str.starts_with("req_"),
        "generated ID must start with 'req_', got: {id_str}"
    );
}

/// When the client sends `X-Request-Id`, the middleware echoes it unchanged.
#[tokio::test]
async fn request_id_echoed_when_present() {
    let app = test_router();

    let req = Request::builder()
        .uri("/healthz")
        .header("x-request-id", "req_test123")
        .body(Body::empty())
        .unwrap();

    let res = app.oneshot(req).await.unwrap();

    assert_eq!(res.status(), StatusCode::OK);

    let id_header = res
        .headers()
        .get("x-request-id")
        .expect("response must contain X-Request-Id header");

    assert_eq!(
        id_header.to_str().unwrap(),
        "req_test123",
        "middleware must echo the caller-supplied ID unchanged"
    );
}
