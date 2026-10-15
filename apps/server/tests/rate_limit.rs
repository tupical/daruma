mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use daruma_auth::{generate, NewTokenSpec, TokenKind, TokenScope};
use daruma_shared::AgentId;
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
                .uri("/v1/tasks?status=all")
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
                .uri("/v1/tasks?status=all")
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

async fn post_mcp(
    app: &common::TestApp,
    token: &str,
    uri: &str,
    body: serde_json::Value,
) -> StatusCode {
    app.router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .header("host", "127.0.0.1:9")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

fn rpc(id: u64, method: &str) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {}})
}

#[tokio::test]
async fn mcp_handshake_does_not_spend_the_tool_call_budget() {
    let app = common::test_app().await;
    let token = mint_limited_token(&app, "tenant-handshake", 1).await;

    // A session-per-call client: initialize, server/discover, notification.
    // All three ride the handshake bucket, not the shared one.
    assert_eq!(
        post_mcp(&app, &token, "/v1/mcp", rpc(1, "initialize")).await,
        StatusCode::OK
    );
    assert_eq!(get_tasks(&app, &token).await, StatusCode::OK);
    assert_eq!(get_tasks(&app, &token).await, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn mcp_handshake_bucket_is_bounded_per_token() {
    let app = common::test_app().await;
    let token = mint_limited_token(&app, "tenant-handshake-flood", 1).await;

    assert_eq!(
        post_mcp(&app, &token, "/v1/mcp", rpc(1, "initialize")).await,
        StatusCode::OK
    );
    let notification = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    assert_eq!(
        post_mcp(&app, &token, "/mcp", notification).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // The flood never touched the shared budget.
    assert_eq!(get_tasks(&app, &token).await, StatusCode::OK);
}

#[tokio::test]
async fn mcp_tool_call_and_mixed_batch_spend_the_shared_budget() {
    let app = common::test_app().await;
    let call = mint_limited_token(&app, "tenant-call", 1).await;
    let batch = mint_limited_token(&app, "tenant-batch", 1).await;

    let tool_call = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "no_such_tool", "arguments": {}}
    });
    assert_ne!(
        post_mcp(&app, &call, "/v1/mcp", tool_call.clone()).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(get_tasks(&app, &call).await, StatusCode::TOO_MANY_REQUESTS);

    let mixed = serde_json::json!([rpc(1, "initialize"), tool_call]);
    assert_ne!(
        post_mcp(&app, &batch, "/v1/mcp", mixed).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(get_tasks(&app, &batch).await, StatusCode::TOO_MANY_REQUESTS);
    // The handshake bucket of that token is still full.
    assert_eq!(
        post_mcp(&app, &batch, "/v1/mcp", rpc(2, "server/discover")).await,
        StatusCode::OK
    );
}
