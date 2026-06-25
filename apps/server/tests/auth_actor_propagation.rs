//! Integration tests for §3.5 — actor propagation through auth context.
//!
//! Covers:
//! * AC-1 — bot token → `Actor::Agent` is attributed to resulting events.
//! * AC-2 — PAT token  → `Actor::User`  is attributed to resulting events.
//! * AC-3 — Without `actor_strict` (default): explicit actor in command envelope
//!   passes through unchanged. With `actor_strict`: individual route
//!   handlers (actor_from) always derive the actor from the token.

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use daruma_auth::{Capabilities, Capability, TokenKind};
use daruma_shared::ProjectId;
use tower::ServiceExt;

mod common;
use common::{mint_with_caps, test_app};

/// POST /v1/plans — a route that uses `actor_from(&auth, None)`, so the actor
/// is always derived from the token rather than the request body.
async fn post_plan(app: axum::Router, token: &str, project_id: ProjectId) -> StatusCode {
    let body = serde_json::json!({
        "plan": {
            "project_id": project_id,
            "title": "Actor test plan",
            "owner": { "kind": "user" }
        }
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/plans")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

// ── AC-1: bot token → Agent actor ────────────────────────────────────────────

#[tokio::test]
async fn ac1_bot_token_produces_agent_actor_in_event() {
    let h = test_app().await;
    let caps: Capabilities = [Capability::PlanWrite, Capability::PlanRead].into();
    let (token, _agent_id) = mint_with_caps(&h.auth_store(), TokenKind::Bot, caps).await;

    let status = post_plan(h.router, &token, ProjectId::new()).await;
    assert_eq!(status, StatusCode::CREATED, "plan creation should succeed");

    // The last persisted event must carry an Agent actor (from bot token).
    let events = h.state.store.load_since(0, 100).await.unwrap();
    let last = events.last().expect("at least one event should be stored");
    assert!(
        last.actor.is_agent(),
        "bot token must produce Actor::Agent; got: {:?}",
        last.actor
    );
    // The agent name must encode the bot token convention: "bot.<agent_id>".
    if let daruma_domain::Actor::Agent { name, .. } = &last.actor {
        assert!(
            name.starts_with("bot."),
            "agent name should start with 'bot.'; got: {name}"
        );
    }
}

// ── AC-2: PAT token → User actor ─────────────────────────────────────────────

#[tokio::test]
async fn ac2_pat_token_produces_user_actor_in_event() {
    let h = test_app().await;
    let caps: Capabilities = [Capability::PlanWrite, Capability::PlanRead].into();
    let (token, _) = mint_with_caps(&h.auth_store(), TokenKind::Pat, caps).await;

    let status = post_plan(h.router, &token, ProjectId::new()).await;
    assert_eq!(status, StatusCode::CREATED, "plan creation should succeed");

    let events = h.state.store.load_since(0, 100).await.unwrap();
    let last = events.last().expect("at least one event should be stored");
    assert_eq!(
        last.actor,
        daruma_domain::Actor::User,
        "PAT token must produce Actor::User"
    );
}

// ── AC-3: actor_strict behaviour ─────────────────────────────────────────────

/// Without `actor_strict` (default): a bot token may supply an explicit
/// `Actor::User` in the `CommandEnvelope` and the `/v1/commands` handler
/// uses it as-is (envelope.actor passes through unchanged).
#[tokio::test]
#[cfg(not(feature = "actor_strict"))]
async fn ac3_no_strict_bot_envelope_actor_user_passes_through() {
    let h = test_app().await;
    let caps: Capabilities = [Capability::TaskWrite, Capability::TaskRead].into();
    let (token, _) = mint_with_caps(&h.auth_store(), TokenKind::Bot, caps).await;

    // Explicitly supply Actor::User in the envelope from a bot token.
    let body = serde_json::json!({
        "command": { "type": "create_task", "task": { "title": "AC-3 task" } },
        "actor": { "kind": "user" }
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/commands")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();
    let res = h.router.oneshot(req).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "command should succeed in non-strict mode"
    );

    // Without actor_strict the envelope actor wins: expect User.
    let events = h.state.store.load_since(0, 100).await.unwrap();
    let last = events.last().expect("event should be stored");
    assert_eq!(
        last.actor,
        daruma_domain::Actor::User,
        "without actor_strict, envelope Actor::User from bot token must pass through"
    );
}

/// With `actor_strict` enabled: individual route handlers (those using
/// `actor_from`) always use the token-derived actor. A bot token consistently
/// produces `Actor::Agent` regardless of what the request body claims.
#[tokio::test]
#[cfg(feature = "actor_strict")]
async fn ac3_strict_route_handler_always_uses_token_derived_actor() {
    let h = test_app().await;
    let caps: Capabilities = [Capability::PlanWrite, Capability::PlanRead].into();
    let (token, _) = mint_with_caps(&h.auth_store(), TokenKind::Bot, caps).await;

    // The plan body sets owner: Actor::User, but actor_from ignores it in
    // strict mode — the token-derived Agent actor is always used.
    let status = post_plan(h.router, &token, ProjectId::new()).await;
    assert_eq!(status, StatusCode::CREATED);

    let events = h.state.store.load_since(0, 100).await.unwrap();
    let last = events.last().expect("event should be stored");
    assert!(
        last.actor.is_agent(),
        "with actor_strict, bot token must always produce Actor::Agent; got: {:?}",
        last.actor
    );
}
