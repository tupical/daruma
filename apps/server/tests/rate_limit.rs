mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use taskagent_auth::{generate, NewTokenSpec, TokenKind, TokenScope};
use taskagent_shared::AgentId;
use tower::ServiceExt;

async fn mint_limited_token(
    app: &common::TestApp,
    tenant_id: &str,
    rate_limit_per_min: u32,
) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT OR IGNORE INTO tenants (id, name, status, created_at, updated_at) \
         VALUES (?, ?, 'active', ?, ?)",
    )
    .bind(tenant_id)
    .bind(tenant_id)
    .bind(&now)
    .bind(&now)
    .execute(app.state.projects.pool())
    .await
    .unwrap();

    let mut secret = generate(NewTokenSpec {
        kind: TokenKind::Pat,
        agent_id: AgentId::new(),
        scope: TokenScope::admin(),
        rate_limit_per_min,
        expired_at: None,
    })
    .unwrap();
    secret.record.tenant_id = Some(tenant_id.to_string());
    app.auth_store().insert(secret.record).await.unwrap();
    secret.plaintext
}

async fn get_tasks(app: &common::TestApp, token: &str) -> StatusCode {
    app.router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/tasks")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn returns_429_with_retry_after_when_token_bucket_is_empty() {
    let app = common::test_app().await;
    let token = mint_limited_token(&app, "tenant-a", 1).await;

    assert_eq!(get_tasks(&app, &token).await, StatusCode::OK);
    let response = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/tasks")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().get("retry-after").is_some());
}

#[tokio::test]
async fn noisy_tenant_does_not_block_another_tenant() {
    let app = common::test_app().await;
    let noisy = mint_limited_token(&app, "tenant-noisy", 1).await;
    let quiet = mint_limited_token(&app, "tenant-quiet", 1).await;

    assert_eq!(get_tasks(&app, &noisy).await, StatusCode::OK);
    assert_eq!(get_tasks(&app, &noisy).await, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(get_tasks(&app, &quiet).await, StatusCode::OK);
}

#[tokio::test]
async fn healthz_is_not_rate_limited() {
    let app = common::test_app().await;

    for _ in 0..5 {
        let response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
