//! AC-10 — the target multi-agent realtime scenario from
//! `.omc/plans/section-e-multi-agent-realtime.md` §1:
//!
//! > Agent A closes a task T (`SetStatus → Done`). A human reopens T
//! > (`SetStatus → Todo`) and adds a comment. Every subscribed agent
//! > (including A and any other) receives `task.reopened` and
//! > `task.commented` within < 1 s, without any direct message.
//!
//! Concretely the test wires:
//!   * **Agent A** — WS client, subscribed to channels `[tasks, comments]`,
//!     drives `CompleteTask` over the WS Dispatch path.
//!   * **Agent B** — WS client, subscribed the same way; only listens.
//!   * **Human** — HTTP client driving `POST /v1/commands` for reopen and
//!     `POST /v1/tasks/{id}/comments` for the comment.
//!
//! After the reopen + comment, both A and B must observe `task_reopened`
//! and `task_commented` frames within a 1-second budget.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::{connect_async, tungstenite::Message};

mod common;
use common::{spawn_server as spawn_test_server, test_app};

// ── Harness ─────────────────────────────────────────────────────────────────

struct E2EServer {
    addr: SocketAddr,
    token: String,
}

async fn spawn_server() -> E2EServer {
    let app = test_app().await;
    let addr = spawn_test_server(&app).await;
    E2EServer {
        addr,
        token: app.admin_token,
    }
}

type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type WsStream = futures::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

async fn ws_connect(addr: SocketAddr, token: &str) -> (WsSink, WsStream) {
    let url = format!("ws://{addr}/v1/ws?token={token}");
    let (stream, _resp) = connect_async(&url).await.expect("WS connect");
    stream.split()
}

async fn next_text(stream: &mut WsStream, timeout: Duration) -> Option<Value> {
    let frame = tokio::time::timeout(timeout, stream.next())
        .await
        .ok()??
        .ok()?;
    let text = match frame {
        Message::Text(t) => t.to_string(),
        _ => return None,
    };
    serde_json::from_str(&text).ok()
}

/// Pull frames until one matches `kind` (in `envelope.payload.type`) or
/// the deadline expires. Returns the matching frame, or `None` on timeout.
async fn wait_for_kind(stream: &mut WsStream, kind: &str, budget: Duration) -> Option<Value> {
    let deadline = Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let frame = next_text(stream, remaining).await?;
        if frame["type"] == "event" && frame["envelope"]["payload"]["type"] == kind {
            return Some(frame);
        }
    }
}

async fn http_post_json(client: &reqwest::Client, url: &str, token: &str, body: Value) -> Value {
    let resp = client
        .post(url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .expect("POST OK");
    let status = resp.status();
    let json = resp.json::<Value>().await.unwrap_or(Value::Null);
    assert!(
        status.is_success(),
        "POST {url} failed: status={status}, body={json}"
    );
    json
}

// ── AC-10 ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn ac10_multi_agent_realtime_reopen_and_comment() {
    let server = spawn_server().await;
    let http = reqwest::Client::new();
    let server_url = format!("http://{}", server.addr);

    // ── 1. Connect both agents and subscribe to tasks + comments ───────────
    let (mut a_sink, mut a_stream) = ws_connect(server.addr, &server.token).await;
    let (mut b_sink, mut b_stream) = ws_connect(server.addr, &server.token).await;

    // Drain the Hello frames.
    assert_eq!(
        next_text(&mut a_stream, Duration::from_secs(2))
            .await
            .unwrap()["type"],
        "hello"
    );
    assert_eq!(
        next_text(&mut b_stream, Duration::from_secs(2))
            .await
            .unwrap()["type"],
        "hello"
    );

    for sink in [&mut a_sink, &mut b_sink] {
        sink.send(Message::Text(
            json!({"type":"subscribe","channels":["tasks","comments"]})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    }
    // Allow the live forwarder tasks to wire up.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // ── 2. Human creates the task over HTTP ─────────────────────────────────
    let create = http_post_json(
        &http,
        &format!("{server_url}/v1/commands"),
        &server.token,
        json!({"command":{"type":"create_task","task":{"title":"AC-10 target"}}}),
    )
    .await;
    let task_id = create["data"]
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
        .expect("task_created carries an id");

    // Both clients see the creation. (Not part of the timing assertion —
    // just confirms the subscription is live.)
    assert!(
        wait_for_kind(&mut a_stream, "task_created", Duration::from_secs(2))
            .await
            .is_some(),
        "A must see task_created"
    );
    assert!(
        wait_for_kind(&mut b_stream, "task_created", Duration::from_secs(2))
            .await
            .is_some(),
        "B must see task_created"
    );

    // ── 3. Agent A closes the task via the WS Dispatch path ─────────────────
    a_sink
        .send(Message::Text(
            json!({"type":"dispatch","command":{"type":"complete_task","id": task_id}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    // Both clients observe the close transition.
    assert!(
        wait_for_kind(&mut a_stream, "task_closed", Duration::from_secs(2))
            .await
            .is_some(),
        "A must see task_closed (its own action)"
    );
    assert!(
        wait_for_kind(&mut b_stream, "task_closed", Duration::from_secs(2))
            .await
            .is_some(),
        "B must see task_closed (other agent's action)"
    );

    // ── 4. Human reopens + comments (over HTTP); measure delivery ──────────
    let t0 = Instant::now();

    // Reopen — SetStatus from Done → Todo emits TaskStatusChanged + TaskReopened.
    http_post_json(
        &http,
        &format!("{server_url}/v1/commands"),
        &server.token,
        json!({"command":{"type":"set_status","id": task_id, "status": "todo"}}),
    )
    .await;

    // Comment — emits CommentAdded + TaskCommented.
    http_post_json(
        &http,
        &format!("{server_url}/v1/tasks/{task_id}/comments"),
        &server.token,
        json!({"body": "AC-10 follow-up comment"}),
    )
    .await;

    // ── 5. Both agents observe both semantic events within < 1 s ───────────
    let budget = Duration::from_secs(1);
    let a_reopen = wait_for_kind(&mut a_stream, "task_reopened", budget).await;
    let a_commented = wait_for_kind(&mut a_stream, "task_commented", budget).await;
    let b_reopen = wait_for_kind(&mut b_stream, "task_reopened", budget).await;
    let b_commented = wait_for_kind(&mut b_stream, "task_commented", budget).await;
    let elapsed = t0.elapsed();

    assert!(a_reopen.is_some(), "A must see task_reopened within 1 s");
    assert!(
        a_commented.is_some(),
        "A must see task_commented within 1 s"
    );
    assert!(b_reopen.is_some(), "B must see task_reopened within 1 s");
    assert!(
        b_commented.is_some(),
        "B must see task_commented within 1 s"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "every WS delivery must complete in < 2 s (got {elapsed:?})"
    );

    // 6. Sanity-check the semantic event payload — task_commented carries a
    // preview and pointer back to the comment row.
    let commented = b_commented.unwrap();
    let preview = commented["envelope"]["payload"]["preview"]
        .as_str()
        .expect("task_commented.preview");
    assert!(
        preview.starts_with("AC-10 follow-up"),
        "preview must echo the body"
    );
    assert!(
        commented["envelope"]["payload"]["comment_id"].is_string(),
        "task_commented carries comment_id"
    );
}
