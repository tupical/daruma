//! AC-8 — per-agent inbox: list events past the agent's cursor, ack to
//! advance the cursor, optional long-poll.

use axum::http::StatusCode;
use taskagent_auth::{generate, Capability, NewTokenSpec, ProjectFilter, TokenKind, TokenScope};
use taskagent_shared::AgentId;

mod common;
use common::{json_get, json_post, TestAppBuilder};

// ── AC-8 ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn ac8_inbox_returns_events_and_ack_advances_cursor() {
    let h = TestAppBuilder::default().build().await;
    let inbox_uri = format!("/v1/agents/{}/inbox", h.admin_agent_id.as_uuid());

    // 1. Create a task — produces TaskCreated (seq = 1).
    let (status, _) = json_post(
        h.router.clone(),
        &h.admin_token,
        "/v1/commands",
        r#"{"command":{"type":"create_task","task":{"title":"inbox AC-8"}}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 2. Fresh cursor (0) — list should contain at least one event.
    let (status, list) = json_get(h.router.clone(), &h.admin_token, &inbox_uri).await;
    assert_eq!(status, StatusCode::OK);
    let arr = list.as_array().expect("inbox response is an array");
    assert!(!arr.is_empty(), "expected at least one event in the inbox");
    let max_seq = arr
        .iter()
        .filter_map(|e| e.get("seq").and_then(|v| v.as_u64()))
        .max()
        .unwrap();

    // 3. Ack up to that seq.
    let (status, ack) = json_post(
        h.router.clone(),
        &h.admin_token,
        &format!("{inbox_uri}/ack"),
        &format!("{{\"up_to_seq\":{max_seq}}}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ack["last_acked_seq"], max_seq);

    // 4. After ack, with default `since` (cursor), the inbox is empty.
    let (status, list_after) = json_get(h.router.clone(), &h.admin_token, &inbox_uri).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        list_after.as_array().unwrap().is_empty(),
        "inbox must drain after ack"
    );
}

#[tokio::test]
async fn ac8_long_poll_returns_empty_after_timeout() {
    let h = TestAppBuilder::default().build().await;
    let inbox_uri = format!(
        "/v1/agents/{}/inbox?long_poll=1&since=999999",
        h.admin_agent_id.as_uuid()
    );

    let start = std::time::Instant::now();
    let (status, list) = json_get(h.router.clone(), &h.admin_token, &inbox_uri).await;
    let elapsed = start.elapsed();
    assert_eq!(status, StatusCode::OK);
    assert!(list.as_array().unwrap().is_empty());
    assert!(
        elapsed >= std::time::Duration::from_millis(900),
        "long-poll must wait close to its budget (got {elapsed:?})"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "long-poll must not exceed its budget by much (got {elapsed:?})"
    );
}

#[tokio::test]
async fn ac8_cannot_read_another_agents_inbox() {
    let h = TestAppBuilder::default().build().await;

    // Mint a non-admin token bound to a *different* agent.
    let other_agent = AgentId::new();
    let other_token = generate(NewTokenSpec {
        kind: TokenKind::Bot,
        agent_id: other_agent,
        scope: TokenScope {
            projects: ProjectFilter::All,
            capabilities: [Capability::TaskRead].into(),
        },
        rate_limit_per_min: 60,
        expired_at: None,
    })
    .unwrap();
    h.auth_store()
        .insert(other_token.record.clone())
        .await
        .unwrap();

    // The other token may not read the *first* agent's inbox.
    let foreign_uri = format!("/v1/agents/{}/inbox", h.admin_agent_id.as_uuid());
    let (status, body) = json_get(h.router.clone(), &other_token.plaintext, &foreign_uri).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "forbidden");
}
