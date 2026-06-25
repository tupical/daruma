//! End-to-end integration tests for task relation HTTP endpoints (§3.2 W3.1).
//!
//! Covers AC-1, AC-2, AC-6, AC-11, AC-12:
//!   POST   /v1/relations           → 201 + MutationResponse(relation_id, event_id, event_seq)
//!   GET    /v1/tasks/{id}/relations → 200 + five-group TaskRelations shape
//!   DELETE /v1/relations/{id}      → 200 + MutationResponse(relation_id)
//!   Idempotent POST via client_command_id
//!   Capability gating (403 without TaskRelationWrite)
//!   Cycle detection (400 on cycle)

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use serde_json::Value;
use daruma_auth::{
    generate, Capabilities, Capability, NewTokenSpec, ProjectFilter, TokenKind, TokenScope,
    TokenStore,
};
use daruma_events::EventStore;
use tower::ServiceExt;

mod common;
use common::test_app;

// ── Harness ───────────────────────────────────────────────────────────────────

struct Harness {
    app: axum::Router,
    auth_store: Arc<dyn TokenStore>,
    store: Arc<dyn EventStore>,
    token: String,
}

async fn build_harness() -> Harness {
    let h = test_app().await;
    Harness {
        app: h.router,
        auth_store: h.state.auth_store.clone(),
        store: h.state.store.clone(),
        token: h.admin_token,
    }
}

/// Mint a token with specific capabilities.
async fn mint_token(store: &Arc<dyn TokenStore>, caps: Capabilities) -> String {
    let secret = generate(NewTokenSpec {
        kind: TokenKind::Pat,
        agent_id: daruma_shared::AgentId::new(),
        scope: TokenScope {
            projects: ProjectFilter::All,
            capabilities: caps,
        },
        rate_limit_per_min: 60,
        expired_at: None,
    })
    .unwrap();
    store.insert(secret.record.clone()).await.unwrap();
    secret.plaintext
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

async fn post_json(app: &axum::Router, token: &str, uri: &str, body: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn get_json(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn delete_json(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::DELETE)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Create a task and return its UUID string.
async fn create_task(app: &axum::Router, token: &str, title: &str) -> String {
    let body = format!(r#"{{"command":{{"type":"create_task","task":{{"title":"{title}"}}}}}}"#);
    let (s, ev) = post_json(app, token, "/v1/commands", &body).await;
    assert_eq!(s, StatusCode::OK, "create_task failed: {ev}");
    ev["data"]
        .as_array()
        .expect("data array")
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == "task_created" {
                p.get("task")?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("task_created event with task.id")
}

// ── AC-1: POST /v1/relations emits TaskLinked event ───────────────────────────

/// AC-1: POST /v1/relations → 201, body has relation_id + event_id + event_seq.
/// The event store must contain exactly one TaskLinked{from, to, kind: Blocks}.
#[tokio::test]
async fn create_relation_emits_event() {
    let h = build_harness().await;
    let from = create_task(&h.app, &h.token, "Blocker").await;
    let to = create_task(&h.app, &h.token, "Blocked").await;

    let body = format!(r#"{{"from":"{from}","to":"{to}","kind":"blocks"}}"#);
    let (status, resp) = post_json(&h.app, &h.token, "/v1/relations", &body).await;

    assert_eq!(status, StatusCode::CREATED, "expected 201: {resp}");
    assert_eq!(resp["success"], true, "success must be true: {resp}");
    assert!(
        resp["event_id"].is_string(),
        "event_id must be present: {resp}"
    );
    assert!(
        resp["event_seq"].is_number(),
        "event_seq must be present: {resp}"
    );
    let relation_id = resp["data"]["relation_id"]
        .as_str()
        .expect("data.relation_id must be a string");
    assert!(!relation_id.is_empty(), "relation_id must not be empty");

    // Verify TaskLinked event in the store.
    let events = h.store.load_since(0, 100).await.unwrap();
    let linked = events.iter().find(|e| e.payload.kind() == "task.linked");
    assert!(linked.is_some(), "TaskLinked event must be in the store");
    let ev = linked.unwrap();
    let json_payload = serde_json::to_value(&ev.payload).unwrap();
    assert_eq!(
        json_payload["from"].as_str().unwrap(),
        from,
        "from must match"
    );
    assert_eq!(json_payload["to"].as_str().unwrap(), to, "to must match");
    assert_eq!(
        json_payload["kind"].as_str().unwrap(),
        "blocks",
        "kind must be blocks"
    );
}

// ── AC-2: GET /v1/tasks/{id}/relations returns five groups ────────────────────

/// AC-2: Create relations of all 3 kinds from task A, then GET A's relations.
/// Result must have correct distribution across all five groups.
#[tokio::test]
async fn list_returns_five_groups() {
    let h = build_harness().await;
    let a = create_task(&h.app, &h.token, "Task A").await;
    let b = create_task(&h.app, &h.token, "Task B").await;
    let c = create_task(&h.app, &h.token, "Task C").await;
    let d = create_task(&h.app, &h.token, "Task D").await;

    // A blocks B
    let (s1, _) = post_json(
        &h.app,
        &h.token,
        "/v1/relations",
        &format!(r#"{{"from":"{a}","to":"{b}","kind":"blocks"}}"#),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    // A relates_to C
    let (s2, _) = post_json(
        &h.app,
        &h.token,
        "/v1/relations",
        &format!(r#"{{"from":"{a}","to":"{c}","kind":"relates_to"}}"#),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED);

    // A duplicates D
    let (s3, _) = post_json(
        &h.app,
        &h.token,
        "/v1/relations",
        &format!(r#"{{"from":"{a}","to":"{d}","kind":"duplicates"}}"#),
    )
    .await;
    assert_eq!(s3, StatusCode::CREATED);

    let (gs, body) = get_json(&h.app, &h.token, &format!("/v1/tasks/{a}/relations")).await;
    assert_eq!(gs, StatusCode::OK, "GET relations failed: {body}");

    assert_eq!(
        body["blocks"].as_array().unwrap().len(),
        1,
        "blocks must have 1: {body}"
    );
    assert_eq!(
        body["blocked_by"].as_array().unwrap().len(),
        0,
        "blocked_by must be empty: {body}"
    );
    assert_eq!(
        body["relates_to"].as_array().unwrap().len(),
        1,
        "relates_to must have 1: {body}"
    );
    assert_eq!(
        body["duplicates"].as_array().unwrap().len(),
        1,
        "duplicates must have 1: {body}"
    );
    assert_eq!(
        body["duplicated_by"].as_array().unwrap().len(),
        0,
        "duplicated_by must be empty: {body}"
    );
}

// ── Bulk read: POST /v1/relations/query avoids long query strings ─────────────

/// POST /v1/relations/query returns the same flat bulk projection as the legacy
/// GET /v1/relations?task_ids=... endpoint, without putting every id in the URL.
#[tokio::test]
async fn query_relations_for_tasks_uses_json_body() {
    let h = build_harness().await;
    let a = create_task(&h.app, &h.token, "Bulk Query A").await;
    let b = create_task(&h.app, &h.token, "Bulk Query B").await;
    let c = create_task(&h.app, &h.token, "Bulk Query C").await;

    let (s1, _) = post_json(
        &h.app,
        &h.token,
        "/v1/relations",
        &format!(r#"{{"from":"{a}","to":"{b}","kind":"blocks"}}"#),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    let body = format!(r#"{{"task_ids":["{a}","{c}"]}}"#);
    let (status, resp) = post_json(&h.app, &h.token, "/v1/relations/query", &body).await;

    assert_eq!(status, StatusCode::OK, "bulk query failed: {resp}");
    let rels = resp.as_array().expect("relations array");
    assert_eq!(rels.len(), 1, "expected only A/B relation: {resp}");
    assert_eq!(rels[0]["from"].as_str().unwrap(), a);
    assert_eq!(rels[0]["to"].as_str().unwrap(), b);
}

// ── AC-6: DELETE /v1/relations/{id} emits TaskUnlinked ────────────────────────

/// AC-6: Create a relation, then DELETE it. Response is 200 + MutationResponse.
/// The event store must contain a TaskUnlinked event.
#[tokio::test]
async fn delete_emits_unlinked() {
    let h = build_harness().await;
    let from = create_task(&h.app, &h.token, "From").await;
    let to = create_task(&h.app, &h.token, "To").await;

    let body = format!(r#"{{"from":"{from}","to":"{to}","kind":"relates_to"}}"#);
    let (_, create_resp) = post_json(&h.app, &h.token, "/v1/relations", &body).await;
    let relation_id = create_resp["data"]["relation_id"]
        .as_str()
        .expect("relation_id from create")
        .to_owned();

    let (del_status, del_resp) =
        delete_json(&h.app, &h.token, &format!("/v1/relations/{relation_id}")).await;

    assert_eq!(
        del_status,
        StatusCode::OK,
        "DELETE must return 200: {del_resp}"
    );
    assert_eq!(
        del_resp["success"], true,
        "success must be true: {del_resp}"
    );
    assert!(
        del_resp["event_id"].is_string(),
        "event_id must be present: {del_resp}"
    );
    assert!(
        del_resp["event_seq"].is_number(),
        "event_seq must be present: {del_resp}"
    );
    assert_eq!(
        del_resp["data"]["relation_id"].as_str().unwrap(),
        relation_id,
        "relation_id echo must match"
    );

    // Verify TaskUnlinked in store.
    let events = h.store.load_since(0, 100).await.unwrap();
    let unlinked = events.iter().any(|e| e.payload.kind() == "task.unlinked");
    assert!(unlinked, "TaskUnlinked event must appear in the store");
}

// ── AC-11: Idempotent link via client_command_id ──────────────────────────────

/// AC-11: Same POST with identical client_command_id twice → same event_id,
/// exactly 1 TaskLinked event in the store.
#[tokio::test]
async fn idempotent_link_via_ccid() {
    let h = build_harness().await;
    let from = create_task(&h.app, &h.token, "Idem From").await;
    let to = create_task(&h.app, &h.token, "Idem To").await;
    let ccid = uuid::Uuid::new_v4();

    let body =
        format!(r#"{{"from":"{from}","to":"{to}","kind":"blocks","client_command_id":"{ccid}"}}"#);

    let (s1, r1) = post_json(&h.app, &h.token, "/v1/relations", &body).await;
    assert_eq!(s1, StatusCode::CREATED, "first call failed: {r1}");
    assert_eq!(r1["success"], true);
    let event_id_1 = r1["event_id"]
        .as_str()
        .expect("event_id on first call")
        .to_owned();

    let (s2, r2) = post_json(&h.app, &h.token, "/v1/relations", &body).await;
    assert_eq!(s2, StatusCode::CREATED, "second call failed: {r2}");
    assert_eq!(r2["success"], true);
    let event_id_2 = r2["event_id"]
        .as_str()
        .expect("event_id on second call")
        .to_owned();

    assert_eq!(
        event_id_1, event_id_2,
        "same ccid must return the same event_id"
    );
    assert_eq!(
        r1["data"]["relation_id"], r2["data"]["relation_id"],
        "cached replay must preserve relation_id"
    );
    assert_eq!(
        r2["client_command_id"].as_str().unwrap(),
        ccid.to_string(),
        "client_command_id must be echoed"
    );

    // Store must contain exactly 1 TaskLinked event (no duplicate).
    let events = h.store.load_since(0, 100).await.unwrap();
    let linked_count = events
        .iter()
        .filter(|e| e.payload.kind() == "task.linked")
        .count();
    assert_eq!(
        linked_count, 1,
        "exactly 1 TaskLinked event expected, got {linked_count}"
    );
}

// ── AC-12: MutationResponse shape for link and unlink ────────────────────────

/// AC-12: POST returns {success, event_id, event_seq, data: {relation_id}, client_command_id}.
/// DELETE returns the same shape.
#[tokio::test]
async fn response_shape_link_unlink() {
    let h = build_harness().await;
    let from = create_task(&h.app, &h.token, "Shape From").await;
    let to = create_task(&h.app, &h.token, "Shape To").await;
    let ccid = uuid::Uuid::new_v4();

    // POST — check shape.
    let link_body = format!(
        r#"{{"from":"{from}","to":"{to}","kind":"relates_to","client_command_id":"{ccid}"}}"#
    );
    let (ls, link_resp) = post_json(&h.app, &h.token, "/v1/relations", &link_body).await;
    assert_eq!(ls, StatusCode::CREATED, "POST must return 201: {link_resp}");

    assert_eq!(link_resp["success"], true, "success: {link_resp}");
    assert!(
        link_resp["event_id"].is_string(),
        "event_id is string: {link_resp}"
    );
    assert!(
        link_resp["event_seq"].is_number(),
        "event_seq is number: {link_resp}"
    );
    assert!(
        link_resp["data"]["relation_id"].is_string(),
        "data.relation_id is string: {link_resp}"
    );
    assert_eq!(
        link_resp["client_command_id"].as_str().unwrap(),
        ccid.to_string(),
        "client_command_id echoed: {link_resp}"
    );

    // DELETE — check shape.
    let relation_id = link_resp["data"]["relation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (ds, del_resp) =
        delete_json(&h.app, &h.token, &format!("/v1/relations/{relation_id}")).await;
    assert_eq!(ds, StatusCode::OK, "DELETE must return 200: {del_resp}");

    assert_eq!(del_resp["success"], true, "success: {del_resp}");
    assert!(
        del_resp["event_id"].is_string(),
        "event_id is string: {del_resp}"
    );
    assert!(
        del_resp["event_seq"].is_number(),
        "event_seq is number: {del_resp}"
    );
    assert!(
        del_resp["data"]["relation_id"].is_string(),
        "data.relation_id is string: {del_resp}"
    );
}

// ── Bonus: 403 without TaskRelationWrite ──────────────────────────────────────

/// A token with only TaskRelationRead (not Write) must get 403 on POST.
#[tokio::test]
async fn link_returns_403_without_write_cap() {
    let h = build_harness().await;
    let from = create_task(&h.app, &h.token, "Cap From").await;
    let to = create_task(&h.app, &h.token, "Cap To").await;

    let read_only = mint_token(&h.auth_store, [Capability::TaskRelationRead].into()).await;
    let body = format!(r#"{{"from":"{from}","to":"{to}","kind":"blocks"}}"#);
    let (status, resp) = post_json(&h.app, &read_only, "/v1/relations", &body).await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "read-only token must get 403: {resp}"
    );
    assert_eq!(
        resp["error"]["code"], "forbidden",
        "error code must be forbidden: {resp}"
    );
}

// ── AC-10: Webhook delivery for task.unblocked ───────────────────────────────

/// AC-10: When blocker A is set Done and B becomes unblocked, the webhook
/// dispatcher must POST to the subscriber URL with:
///   - `X-Daruma-Event: task.unblocked`
///   - valid HMAC in `X-Daruma-Signature`
///   - payload `{ "payload": { "type": "task_unblocked", ... } }`
///
/// This test spawns a real TCP mock receiver alongside the real server so the
/// dispatcher's `reqwest::Client` can reach it.
#[tokio::test]
async fn webhook_emits_task_unblocked() {
    use axum::{
        extract::State as AxState, http::HeaderMap, routing::post as axpost, Router as AxRouter,
    };
    use std::sync::Mutex;
    use daruma_webhooks::{sign_body_hex, spawn_dispatcher, NoopEnrichment};

    // ── Mock receiver ────────────────────────────────────────────────────────
    #[derive(Debug, Clone)]
    struct MockHit {
        headers: HeaderMap,
        body: Vec<u8>,
    }
    #[derive(Clone, Default)]
    struct MockState {
        hits: Arc<Mutex<Vec<MockHit>>>,
    }
    async fn record_hook(
        AxState(state): AxState<MockState>,
        headers: HeaderMap,
        body: axum::body::Bytes,
    ) -> &'static str {
        state.hits.lock().unwrap().push(MockHit {
            headers,
            body: body.to_vec(),
        });
        "ok"
    }

    let mock_state = MockState::default();
    let mock_app = AxRouter::new()
        .route("/hook", axpost(record_hook))
        .with_state(mock_state.clone());
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.unwrap();
    });

    // ── Real server with dispatcher ──────────────────────────────────────────
    let test = test_app().await;
    let http_client = reqwest::Client::new();
    let _dispatcher = spawn_dispatcher(
        test.bus.subscribe(),
        test.state.webhook_store.clone(),
        http_client.clone(),
        Arc::new(NoopEnrichment),
    );
    let server_addr = common::spawn_server(&test).await;
    let admin_token = test.admin_token.clone();

    let base_url = format!("http://{server_addr}");
    let webhook_secret = "ac10-hmac-secret";

    // Register a webhook subscriber for task.unblocked.
    let raw = http_client
        .post(format!("{base_url}/v1/webhooks"))
        .bearer_auth(&admin_token)
        .json(&serde_json::json!({
            "url": format!("http://{mock_addr}/hook"),
            "secret": webhook_secret,
            "events": ["task.unblocked"],
            "is_active": true,
        }))
        .send()
        .await
        .unwrap();
    let wh_status = raw.status().as_u16();
    let wh_body: serde_json::Value = raw.json().await.unwrap_or(serde_json::Value::Null);
    assert_eq!(wh_status, 201, "POST /v1/webhooks => 201; body: {wh_body}");

    // Create tasks A and B; A blocks B.
    let a_resp = http_client
        .post(format!("{base_url}/v1/commands"))
        .bearer_auth(&admin_token)
        .json(&serde_json::json!({"command": {"type": "create_task", "task": {"title": "AC10 Blocker"}}}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let a_id = a_resp["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == "task_created" {
                p.get("task")?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("task A id");

    let b_resp = http_client
        .post(format!("{base_url}/v1/commands"))
        .bearer_auth(&admin_token)
        .json(&serde_json::json!({"command": {"type": "create_task", "task": {"title": "AC10 Blocked"}}}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let b_id = b_resp["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == "task_created" {
                p.get("task")?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("task B id");

    // A blocks B.
    let rel_raw = http_client
        .post(format!("{base_url}/v1/relations"))
        .bearer_auth(&admin_token)
        .json(&serde_json::json!({"from": a_id, "to": b_id, "kind": "blocks"}))
        .send()
        .await
        .unwrap();
    assert_eq!(rel_raw.status().as_u16(), 201, "POST /v1/relations failed");

    // Set A → Done; this should emit TaskUnblocked(B) and trigger the webhook.
    let done_raw = http_client
        .post(format!("{base_url}/v1/commands"))
        .bearer_auth(&admin_token)
        .json(&serde_json::json!({"command": {"type": "set_status", "id": a_id, "status": "done"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(done_raw.status().as_u16(), 200, "set_status done failed");

    // Wait up to 2 s for the mock to receive the task.unblocked webhook.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let hit = loop {
        {
            let hits = mock_state.hits.lock().unwrap();
            // Find the hit for task.unblocked (webhook may also deliver other events).
            let found = hits.iter().find(|h| {
                h.headers
                    .get("x-daruma-event")
                    .and_then(|v| v.to_str().ok())
                    == Some("task.unblocked")
            });
            if let Some(h) = found {
                break h.clone();
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("mock did not receive task.unblocked webhook within 2 s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };

    // Verify HMAC.
    let sig = hit
        .headers
        .get("x-daruma-signature")
        .expect("X-Daruma-Signature")
        .to_str()
        .unwrap();
    let expected_sig = sign_body_hex(webhook_secret, &hit.body);
    assert_eq!(sig, expected_sig, "HMAC must match");

    // Verify payload kind.
    let envelope: serde_json::Value = serde_json::from_slice(&hit.body).unwrap();
    assert_eq!(
        envelope["payload"]["type"], "task_unblocked",
        "payload type must be task_unblocked"
    );
    assert_eq!(
        envelope["payload"]["task_id"].as_str().unwrap(),
        b_id,
        "task_id in payload must be B"
    );
    assert_eq!(
        envelope["payload"]["unblocked_by"].as_str().unwrap(),
        a_id,
        "unblocked_by in payload must be A"
    );

    // Delivery header must be present.
    assert!(
        hit.headers.contains_key("x-daruma-delivery"),
        "X-Daruma-Delivery must be present"
    );
}

// ── Bonus: Webhook delivery for task.linked ───────────────────────────────────

/// Bonus: POST /v1/relations → webhook task.linked is delivered.
#[tokio::test]
async fn webhook_emits_task_linked() {
    use axum::{
        extract::State as AxState, http::HeaderMap, routing::post as axpost, Router as AxRouter,
    };
    use std::sync::Mutex;
    use daruma_webhooks::{spawn_dispatcher, NoopEnrichment};

    #[derive(Debug, Clone)]
    struct MockHit {
        headers: HeaderMap,
        body: Vec<u8>,
    }
    #[derive(Clone, Default)]
    struct MockState {
        hits: Arc<Mutex<Vec<MockHit>>>,
    }
    async fn record_hook(
        AxState(state): AxState<MockState>,
        headers: HeaderMap,
        body: axum::body::Bytes,
    ) -> &'static str {
        state.hits.lock().unwrap().push(MockHit {
            headers,
            body: body.to_vec(),
        });
        "ok"
    }

    let mock_state = MockState::default();
    let mock_app = AxRouter::new()
        .route("/hook", axpost(record_hook))
        .with_state(mock_state.clone());
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.unwrap();
    });

    let test = test_app().await;
    let http_client = reqwest::Client::new();
    let _dispatcher = spawn_dispatcher(
        test.bus.subscribe(),
        test.state.webhook_store.clone(),
        http_client.clone(),
        Arc::new(NoopEnrichment),
    );
    let server_addr = common::spawn_server(&test).await;
    let admin_token = test.admin_token.clone();

    let base_url = format!("http://{server_addr}");

    // Register webhook for task.linked.
    let wh_raw = http_client
        .post(format!("{base_url}/v1/webhooks"))
        .bearer_auth(&admin_token)
        .json(&serde_json::json!({
            "url": format!("http://{mock_addr}/hook"),
            "secret": "linked-secret",
            "events": ["task.linked"],
            "is_active": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(wh_raw.status().as_u16(), 201, "webhook registration failed");

    // Create tasks and link them.
    let mk_task = |title: &'static str| {
        let client = http_client.clone();
        let url = format!("{base_url}/v1/commands");
        let token = admin_token.clone();
        async move {
            let resp: serde_json::Value = client
                .post(&url)
                .bearer_auth(&token)
                .json(&serde_json::json!({"command": {"type": "create_task", "task": {"title": title}}}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            resp["data"]
                .as_array()
                .unwrap()
                .iter()
                .find_map(|e| {
                    let p = e.get("payload")?;
                    if p.get("type")?.as_str()? == "task_created" {
                        p.get("task")?.get("id")?.as_str().map(str::to_owned)
                    } else {
                        None
                    }
                })
                .expect("task id")
        }
    };

    let from_id = mk_task("WH Linked From").await;
    let to_id = mk_task("WH Linked To").await;

    let link_raw = http_client
        .post(format!("{base_url}/v1/relations"))
        .bearer_auth(&admin_token)
        .json(&serde_json::json!({"from": from_id, "to": to_id, "kind": "relates_to"}))
        .send()
        .await
        .unwrap();
    assert_eq!(link_raw.status().as_u16(), 201, "POST /v1/relations failed");

    // Wait for webhook delivery.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let hit = loop {
        {
            let hits = mock_state.hits.lock().unwrap();
            let found = hits.iter().find(|h| {
                h.headers
                    .get("x-daruma-event")
                    .and_then(|v| v.to_str().ok())
                    == Some("task.linked")
            });
            if let Some(h) = found {
                break h.clone();
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("mock did not receive task.linked webhook within 2 s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };

    let envelope: serde_json::Value = serde_json::from_slice(&hit.body).unwrap();
    assert_eq!(envelope["payload"]["type"], "task_linked");
    assert_eq!(envelope["payload"]["from"].as_str().unwrap(), from_id);
}

// ── Bonus: cycle detection returns 400 ───────────────────────────────────────

/// A→B Blocks, then B→A Blocks must be rejected as a cycle.
#[tokio::test]
async fn link_cycle_returns_400() {
    let h = build_harness().await;
    let a = create_task(&h.app, &h.token, "Cycle A").await;
    let b = create_task(&h.app, &h.token, "Cycle B").await;

    // A blocks B (allowed).
    let (s1, r1) = post_json(
        &h.app,
        &h.token,
        "/v1/relations",
        &format!(r#"{{"from":"{a}","to":"{b}","kind":"blocks"}}"#),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED, "A→B should succeed: {r1}");

    // B blocks A — should fail with cycle detected.
    let (s2, r2) = post_json(
        &h.app,
        &h.token,
        "/v1/relations",
        &format!(r#"{{"from":"{b}","to":"{a}","kind":"blocks"}}"#),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::BAD_REQUEST,
        "B→A cycle must return 400: {r2}"
    );
}
