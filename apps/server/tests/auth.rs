//! Integration tests for the bearer-token auth middleware.
//!
//! Covers Wave 2 / W2.2 acceptance criteria:
//!
//! * AC-5 — requests without `Authorization: Bearer ta_*` to any `/v1/*`
//!   except `/v1/healthz` return 401.
//! * AC-6 — a token without the required capability returns 403.
//!   A token scoped to project `P1` cannot read project `P2`'s tasks
//!   (per-project filtering lands when /v1/tasks gains a `?project_id=`
//!   filter in a later wave).

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use daruma_auth::{
    generate, Capabilities, Capability, NewTokenSpec, ProjectFilter, TokenKind, TokenScope,
};
use daruma_shared::AgentId;
use serde_json::Value;
use tower::ServiceExt;

mod common;
use common::{mint_pat, test_app};

async fn read_json(res: axum::http::Response<Body>) -> Value {
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

// ── AC-5 — unauthenticated requests are rejected with 401 ────────────────────

#[tokio::test]
async fn ac5_no_bearer_returns_401_on_v1_tasks() {
    let h = test_app().await;

    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/tasks?status=all")
        .body(Body::empty())
        .unwrap();
    let res = h.router.oneshot(req).await.unwrap();

    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = read_json(res).await;
    assert_eq!(body["error"]["code"], "auth_missing");
}

#[tokio::test]
async fn ac5_garbage_bearer_returns_401() {
    let h = test_app().await;

    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/tasks?status=all")
        .header("authorization", "Bearer not_a_real_token_at_all")
        .body(Body::empty())
        .unwrap();
    let res = h.router.oneshot(req).await.unwrap();

    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = read_json(res).await;
    assert!(
        body["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .starts_with("auth_"),
        "code should be auth_*, got {:?}",
        body["error"]["code"]
    );
}

#[tokio::test]
async fn ac5_healthz_does_not_require_auth() {
    let h = test_app().await;

    // Root /healthz — never under v1.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let res = h.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // /v1/healthz — public by design.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/healthz")
        .body(Body::empty())
        .unwrap();
    let res = h.router.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

// ── AC-6 — capability gating ──────────────────────────────────────────────────

#[tokio::test]
async fn ac6_token_without_task_write_cannot_create_task() {
    let h = test_app().await;
    let (token, _) = mint_pat(
        &h.auth_store(),
        [Capability::TaskRead, Capability::CommentRead].into(),
        ProjectFilter::All,
    )
    .await;

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/commands")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"command":{"type":"create_task","task":{"title":"AC-6"}}}"#,
        ))
        .unwrap();
    let res = h.router.oneshot(req).await.unwrap();

    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body = read_json(res).await;
    assert_eq!(body["error"]["code"], "forbidden");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("task:write"),
        "message should name the missing capability, got {:?}",
        body["error"]["message"]
    );
}

#[tokio::test]
async fn ac6_tasks_list_without_status_returns_400() {
    let h = test_app().await;
    let (token, _) = mint_pat(
        &h.auth_store(),
        [Capability::TaskRead].into(),
        ProjectFilter::All,
    )
    .await;

    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/tasks")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = h.router.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ac6_token_with_task_read_can_list_tasks() {
    let h = test_app().await;
    let (token, _) = mint_pat(
        &h.auth_store(),
        [Capability::TaskRead].into(),
        ProjectFilter::All,
    )
    .await;

    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/tasks?status=all")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = h.router.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn ac6_revoked_token_is_rejected_with_401() {
    let h = test_app().await;

    // Mint then immediately revoke.
    let secret = generate(NewTokenSpec {
        kind: TokenKind::Pat,
        agent_id: AgentId::new(),
        scope: TokenScope::admin(),
        rate_limit_per_min: 60,
        expired_at: None,
    })
    .unwrap();
    h.auth_store().insert(secret.record.clone()).await.unwrap();
    h.auth_store().revoke(secret.record.id).await.unwrap();

    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/tasks?status=all")
        .header("authorization", format!("Bearer {}", secret.plaintext))
        .body(Body::empty())
        .unwrap();
    let res = h.router.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = read_json(res).await;
    assert_eq!(body["error"]["code"], "auth_revoked");
}

#[tokio::test]
async fn ac6_create_token_requires_token_write() {
    let h = test_app().await;
    let (token, _) = mint_pat(
        &h.auth_store(),
        [Capability::TaskRead].into(), // no token:write
        ProjectFilter::All,
    )
    .await;

    // AgentId serialises transparently over its inner UUID — send the bare
    // UUID, not the `agt_…` display form, so serde-json deserialisation
    // succeeds and the request reaches the capability check.
    let body = serde_json::json!({
        "kind": "pat",
        "agent_id": AgentId::new().as_uuid().to_string(),
        "capabilities": Capabilities::from([Capability::TaskRead]),
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/tokens")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();
    let res = h.router.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}
