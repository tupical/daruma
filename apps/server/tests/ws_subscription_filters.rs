//! §3.7.6 (LIN B.6) — `Subscribe { assignee, verb, parent_plan }` filters.
//!
//! All three filters are optional and ANDed together. Backward-compat:
//! legacy clients that omit the new fields keep the pre-§3.7.6 fan-out.
//!
//! These tests publish directly through the in-process `EventBus` and seed
//! the relevant projections (`plan_tasks`, `agent_claims`) by hand — the
//! WS layer's filter logic is the unit under test, not the command path.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use taskagent_domain::{Actor, NewTask, Plan, PlanStatus, Status};
use taskagent_events::{Event, EventBus, EventEnvelope};
use taskagent_shared::{AgentId, PlanId, ProjectId, TaskId};
use taskagent_storage::{AgentClaimRepo, PlanRepo, TaskRepo};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};

mod common;
use common::{spawn_server as spawn_test_server, test_app};

// ── Server scaffold ───────────────────────────────────────────────────────────

struct B6Server {
    addr: SocketAddr,
    admin_token: String,
    bus: EventBus,
    tasks: Arc<TaskRepo>,
    plans: Arc<PlanRepo>,
    claims: Arc<AgentClaimRepo>,
}

async fn spawn_server() -> B6Server {
    let app = test_app().await;
    let addr = spawn_test_server(&app).await;
    B6Server {
        addr,
        admin_token: app.admin_token,
        bus: app.bus,
        tasks: app.state.tasks.clone(),
        plans: app.state.plans.clone(),
        claims: app.state.claims.clone(),
    }
}

// ── Helper types ──────────────────────────────────────────────────────────────

type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type WsStream = futures::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

async fn connect_ws(addr: SocketAddr, token: &str) -> (WsSink, WsStream) {
    let url = format!("ws://{addr}/v1/ws");
    let mut req = url.into_client_request().expect("WS request");
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        format!("taskagent.v1, bearer.{token}").parse().unwrap(),
    );
    let (stream, resp) = connect_async(req).await.expect("WS connect");
    assert_eq!(
        resp.headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok()),
        Some("taskagent.v1")
    );
    stream.split()
}

async fn next_json(stream: &mut WsStream, timeout: Duration) -> Option<Value> {
    let next = tokio::time::timeout(timeout, stream.next()).await.ok()??;
    let msg = next.ok()?;
    let text = match msg {
        Message::Text(t) => t.to_string(),
        _ => return None,
    };
    serde_json::from_str(&text).ok()
}

/// Drain frames until an `"event"` whose payload's `target_task` matches
/// `task_id`, or `None` on timeout. Ignores hello/snapshot/ping/error.
async fn drain_for_task(
    stream: &mut WsStream,
    task_id: TaskId,
    timeout: Duration,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    let want_uuid = task_id.as_uuid().to_string();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let frame = next_json(stream, remaining).await?;
        if frame.get("type").and_then(|v| v.as_str()) != Some("event") {
            continue;
        }
        let payload = &frame["envelope"]["payload"];
        // Try the most common task-id locations.
        let candidates = [
            payload.get("task_id").and_then(|v| v.as_str()),
            payload
                .get("task")
                .and_then(|t| t.get("id"))
                .and_then(|v| v.as_str()),
        ];
        if candidates.iter().flatten().any(|s| *s == want_uuid) {
            return Some(frame);
        }
    }
}

/// `true` if no `"event"` frame arrives within `timeout` (used to assert
/// negative cases — the filter must drop the event silently).
async fn no_event_in(stream: &mut WsStream, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return true;
        }
        let Some(frame) = next_json(stream, remaining).await else {
            return true;
        };
        if frame.get("type").and_then(|v| v.as_str()) == Some("event") {
            return false;
        }
    }
}

// ── Event builders ────────────────────────────────────────────────────────────

fn task_status_changed_env(task_id: TaskId, seq: u64, from: Status, to: Status) -> EventEnvelope {
    EventEnvelope {
        seq,
        ..EventEnvelope::new(
            Actor::user(),
            Event::TaskStatusChanged { task_id, from, to },
        )
    }
}

fn task_completed_env(task_id: TaskId, seq: u64) -> EventEnvelope {
    use taskagent_shared::time;
    EventEnvelope {
        seq,
        ..EventEnvelope::new(
            Actor::user(),
            Event::TaskCompleted {
                task_id,
                completed_at: time::now(),
            },
        )
    }
}

/// Insert a `Task` row directly so per-task filters that touch the projection
/// have something to look at.
async fn seed_task(tasks: &Arc<TaskRepo>, project_id: ProjectId) -> TaskId {
    let task_id = TaskId::new();
    let mut nt = NewTask::new("seeded");
    nt.id = Some(task_id);
    nt.project_id = Some(project_id);
    let envelope = EventEnvelope::new(Actor::user(), Event::TaskCreated { task: nt });
    tasks.apply_event(&envelope).await.unwrap();
    task_id
}

/// Insert a `Plan` and bind `task_id` to it via the `plan_tasks` projection.
async fn seed_plan_with_task(
    plans: &Arc<PlanRepo>,
    project_id: ProjectId,
    task_id: TaskId,
) -> PlanId {
    use taskagent_shared::time;
    let now = time::now();
    let plan = Plan {
        id: PlanId::new(),
        project_id,
        parent_plan_id: None,
        title: "seeded plan".to_string(),
        description: String::new(),
        goal: String::new(),
        success_criteria: vec![],
        status: PlanStatus::Draft,
        owner: Actor::user(),
        created_at: now,
        updated_at: now,
        archived_at: None,
        source_brief: None,
    };
    plans.insert(&plan).await.unwrap();
    plans.add_task(plan.id, task_id, 0, &[]).await.unwrap();
    plan.id
}

async fn seed_claim(claims: &Arc<AgentClaimRepo>, task_id: TaskId, agent_id: AgentId) {
    let expires_at = Utc::now() + chrono::Duration::seconds(300);
    claims
        .acquire_until(agent_id, task_id, expires_at)
        .await
        .unwrap();
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Subscribe without any of the new fields — receives every event the
/// existing channel/project filters allow. Backward-compat guarantee.
#[tokio::test]
async fn subscribe_without_filters_receives_everything() {
    let server = spawn_server().await;
    let project = ProjectId::new();
    let task_id = seed_task(&server.tasks, project).await;

    let (mut sink, mut stream) = connect_ws(server.addr, &server.admin_token).await;
    let _ = next_json(&mut stream, Duration::from_secs(2)).await; // Hello
    sink.send(Message::Text(
        json!({ "type": "subscribe" }).to_string().into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;

    server.bus.publish(task_status_changed_env(
        task_id,
        1,
        Status::Todo,
        Status::InProgress,
    ));

    assert!(
        drain_for_task(&mut stream, task_id, Duration::from_secs(2))
            .await
            .is_some(),
        "no-filter Subscribe must deliver task event"
    );
}

/// `assignee` narrows by which agent currently holds the claim.
#[tokio::test]
async fn assignee_filter_matches_own_task_only() {
    let server = spawn_server().await;
    let project = ProjectId::new();
    let my_task = seed_task(&server.tasks, project).await;
    let other_task = seed_task(&server.tasks, project).await;

    let me = AgentId::new();
    let someone_else = AgentId::new();
    seed_claim(&server.claims, my_task, me).await;
    seed_claim(&server.claims, other_task, someone_else).await;

    let (mut sink, mut stream) = connect_ws(server.addr, &server.admin_token).await;
    let _ = next_json(&mut stream, Duration::from_secs(2)).await;
    sink.send(Message::Text(
        json!({
            "type": "subscribe",
            "assignee": me.as_uuid().to_string(),
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;

    // Both events are published; only the one for my_task should arrive.
    server.bus.publish(task_status_changed_env(
        other_task,
        1,
        Status::Todo,
        Status::InProgress,
    ));
    server.bus.publish(task_status_changed_env(
        my_task,
        2,
        Status::Todo,
        Status::InProgress,
    ));

    assert!(
        drain_for_task(&mut stream, my_task, Duration::from_secs(2))
            .await
            .is_some(),
        "assignee filter must let through events for own task"
    );
    // No second event should arrive — we already drained the only matching one.
    assert!(
        no_event_in(&mut stream, Duration::from_millis(400)).await,
        "assignee filter must drop events for other agents' tasks"
    );
}

/// `verb` narrows by `payload.type` (i.e. `Event::kind()`).
#[tokio::test]
async fn verb_filter_matches_only_matching_type() {
    let server = spawn_server().await;
    let project = ProjectId::new();
    let task_id = seed_task(&server.tasks, project).await;

    let (mut sink, mut stream) = connect_ws(server.addr, &server.admin_token).await;
    let _ = next_json(&mut stream, Duration::from_secs(2)).await;
    sink.send(Message::Text(
        json!({
            "type": "subscribe",
            "verb": "task_completed",
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;

    // Status change should be filtered out.
    server.bus.publish(task_status_changed_env(
        task_id,
        1,
        Status::Todo,
        Status::Done,
    ));
    // Completion should pass.
    server.bus.publish(task_completed_env(task_id, 2));

    let got = drain_for_task(&mut stream, task_id, Duration::from_secs(2)).await;
    assert!(got.is_some(), "verb filter must let task_completed through");
    assert_eq!(
        got.unwrap()["envelope"]["payload"]["type"]
            .as_str()
            .unwrap_or(""),
        "task_completed"
    );

    // Status-changed should never arrive.
    assert!(
        no_event_in(&mut stream, Duration::from_millis(400)).await,
        "verb filter must drop non-matching kinds"
    );
}

/// `parent_plan` narrows by plan_id via the task → plans projection.
#[tokio::test]
async fn parent_plan_filter_matches_only_plan_tasks() {
    let server = spawn_server().await;
    let project = ProjectId::new();
    let in_plan_task = seed_task(&server.tasks, project).await;
    let lone_task = seed_task(&server.tasks, project).await;
    let plan_id = seed_plan_with_task(&server.plans, project, in_plan_task).await;

    let (mut sink, mut stream) = connect_ws(server.addr, &server.admin_token).await;
    let _ = next_json(&mut stream, Duration::from_secs(2)).await;
    sink.send(Message::Text(
        json!({
            "type": "subscribe",
            "parent_plan": plan_id.as_uuid().to_string(),
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;

    // Task that's NOT in the plan — must be filtered.
    server.bus.publish(task_status_changed_env(
        lone_task,
        1,
        Status::Todo,
        Status::InProgress,
    ));
    // Task that IS in the plan — must pass.
    server.bus.publish(task_status_changed_env(
        in_plan_task,
        2,
        Status::Todo,
        Status::InProgress,
    ));

    assert!(
        drain_for_task(&mut stream, in_plan_task, Duration::from_secs(2))
            .await
            .is_some(),
        "parent_plan filter must let through events for plan tasks"
    );
    assert!(
        no_event_in(&mut stream, Duration::from_millis(400)).await,
        "parent_plan filter must drop events for tasks outside the plan"
    );
}

/// Combined filters are ANDed: only the event satisfying every field is delivered.
#[tokio::test]
async fn combined_filters_are_and() {
    let server = spawn_server().await;
    let project = ProjectId::new();
    let task_in_plan_mine = seed_task(&server.tasks, project).await;
    let task_in_plan_theirs = seed_task(&server.tasks, project).await;
    let me = AgentId::new();
    let other = AgentId::new();
    seed_claim(&server.claims, task_in_plan_mine, me).await;
    seed_claim(&server.claims, task_in_plan_theirs, other).await;
    let plan_id = seed_plan_with_task(&server.plans, project, task_in_plan_mine).await;
    // Bind `task_in_plan_theirs` to a different plan so parent_plan alone
    // wouldn't include it.
    server
        .plans
        .add_task(plan_id, task_in_plan_theirs, 1, &[])
        .await
        .unwrap();

    let (mut sink, mut stream) = connect_ws(server.addr, &server.admin_token).await;
    let _ = next_json(&mut stream, Duration::from_secs(2)).await;
    sink.send(Message::Text(
        json!({
            "type": "subscribe",
            "assignee": me.as_uuid().to_string(),
            "verb": "task_status_changed",
            "parent_plan": plan_id.as_uuid().to_string(),
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;

    // Right plan + right verb + WRONG assignee → drop.
    server.bus.publish(task_status_changed_env(
        task_in_plan_theirs,
        1,
        Status::Todo,
        Status::InProgress,
    ));
    // Right verb + right plan + RIGHT assignee → keep.
    server.bus.publish(task_status_changed_env(
        task_in_plan_mine,
        2,
        Status::Todo,
        Status::InProgress,
    ));
    // Same task & assignee but wrong verb → drop.
    server.bus.publish(task_completed_env(task_in_plan_mine, 3));

    let got = drain_for_task(&mut stream, task_in_plan_mine, Duration::from_secs(2)).await;
    assert!(
        got.is_some(),
        "AND filter must let the fully matching event through"
    );
    assert_eq!(
        got.unwrap()["envelope"]["payload"]["type"]
            .as_str()
            .unwrap_or(""),
        "task_status_changed"
    );

    assert!(
        no_event_in(&mut stream, Duration::from_millis(400)).await,
        "AND filter must drop events failing any single sub-filter"
    );
}

/// Backward-compat: a wire frame without any of the new fields must work
/// identically to before — this exercises `#[serde(default)]` on the
/// optional `assignee` / `verb` / `parent_plan` fields.
#[tokio::test]
async fn legacy_subscribe_without_new_fields_still_works() {
    let server = spawn_server().await;
    let project = ProjectId::new();
    let task_id = seed_task(&server.tasks, project).await;

    let (mut sink, mut stream) = connect_ws(server.addr, &server.admin_token).await;
    let _ = next_json(&mut stream, Duration::from_secs(2)).await;
    // Mimic a pre-§3.7.6 client: only `type` + `channels`.
    sink.send(Message::Text(
        json!({
            "type": "subscribe",
            "channels": ["tasks"]
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;

    server.bus.publish(task_status_changed_env(
        task_id,
        1,
        Status::Todo,
        Status::InProgress,
    ));

    assert!(
        drain_for_task(&mut stream, task_id, Duration::from_secs(2))
            .await
            .is_some(),
        "legacy Subscribe shape must keep the pre-§3.7.6 fan-out"
    );
}
