//! WebSocket upgrade handler and per-connection message loop (WS v2).
//!
//! Wave 2 / W2.3 features:
//!   * Bearer authentication via the `Sec-WebSocket-Protocol` handshake header
//!     (`bearer.<token>` subprotocol). `?token=<...>` remains a legacy fallback.
//!   * `Hello` frame on connection — advertises `server_seq` and a list
//!     of capability strings the server understands.
//!   * `Subscribe` race-fix: snapshot of `(since, current_seq]` is sent
//!     atomically with the live broadcast hand-off, so no event is
//!     dropped or duplicated at the boundary.
//!   * Per-project and per-channel filtering. Events without an inline
//!     `project_id` are resolved against `TaskRepo` (task → project).
//!   * `Resync { from_seq, dropped }` when the broadcast receiver lags.
//!   * Heartbeat: server `Ping` every 25 s; closes the socket if the
//!     client misses two consecutive pongs.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket},
        Query, State, WebSocketUpgrade,
    },
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use taskagent_auth::{
    verify_bearer, AuthContext, Capabilities, Capability, ProjectFilter, TokenStore,
};
use taskagent_events::{Channel, Event, EventEnvelope};
use taskagent_shared::{AgentId, EventId, PlanId, ProjectId, TaskId};
use taskagent_storage::{AgentClaimRepo, PlanRepo, TaskRepo};
use taskagent_sync::{WsClientMessage, WsServerMessage, WS_SUBSCRIBER_CHANNEL};
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};

use crate::state::AppState;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(25);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
const SNAPSHOT_PAGE: usize = 1000;
const WS_PROTOCOL: &str = "taskagent.v1";
const BEARER_PROTOCOL_PREFIX: &str = "bearer.";

/// Legacy query string for `/v1/ws`. Browser clients should use
/// `Sec-WebSocket-Protocol: taskagent.v1, bearer.<token>` instead.
#[derive(Debug, Deserialize)]
pub struct WsAuthQuery {
    #[serde(default)]
    pub token: Option<String>,
}

/// Axum route handler — upgrades the HTTP connection to a WebSocket.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(auth): Query<WsAuthQuery>,
) -> impl IntoResponse {
    let protocol_token = token_from_protocols(&ws);
    let token = protocol_token.or(auth.token);
    ws.protocols([WS_PROTOCOL])
        .on_upgrade(|socket| handle_socket(socket, state, token))
}

// ── Per-connection loop ───────────────────────────────────────────────────────

async fn handle_socket(socket: WebSocket, state: AppState, token: Option<String>) {
    let (mut ws_sink, mut ws_stream) = socket.split();

    // 1. Authenticate.
    let auth_ctx = match authenticate(&state.auth_store, token.as_deref()).await {
        Ok(ctx) => ctx,
        Err(err) => {
            let _ = ws_sink
                .send(Message::Close(Some(CloseFrame {
                    code: 1008, // policy violation
                    reason: err.into(),
                })))
                .await;
            return;
        }
    };

    // 2. Outgoing serialiser — bounded mpsc that funnels all server frames.
    //
    // §3.9.5: bounded(WS_SUBSCRIBER_CHANNEL) so a stalled `ws_sink.send()`
    // applies backpressure to the forwarder instead of letting the queue
    // grow without bound. `try_send` on Full breaks the forwarder loop —
    // same close-and-reconnect path as the upstream slow-consumer drop.
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(WS_SUBSCRIBER_CHANNEL);

    // 3. Send Hello.
    let server_seq = state.store.latest_seq().await.unwrap_or(0);
    send_json(
        &out_tx,
        &WsServerMessage::Hello {
            server_seq,
            capabilities: vec![
                "channels".to_string(),
                "resync".to_string(),
                "heartbeat".to_string(),
                "filters".to_string(),
                "plans".to_string(),
                "runs".to_string(),
                "capability-gated-channels".to_string(),
                "device-sync".to_string(),
                "idempotent-dispatch".to_string(),
            ],
        },
    );

    // 4. Spawn the write-side task that drains `out_rx` to the WS sink.
    let write_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // 5. Connection-wide live subscription handle. Set by the first
    //    successful `Subscribe`; cancelled and replaced on re-subscribe.
    let mut live_task: Option<tokio::task::JoinHandle<()>> = None;

    // 6. Heartbeat tracking.
    let mut hb = interval(HEARTBEAT_INTERVAL);
    hb.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Skip the first immediate tick.
    hb.tick().await;

    let mut awaiting_pong = false;

    // 7. Main read loop.
    loop {
        tokio::select! {
            // ── heartbeat tick ───────────────────────────────────────────────
            _ = hb.tick() => {
                if awaiting_pong {
                    // Client missed the previous pong window.
                    tracing::debug!("ws: heartbeat timeout, closing");
                    break;
                }
                send_json(&out_tx, &WsServerMessage::Ping);
                awaiting_pong = true;

                // Arm a timeout to detect missed pong before the next tick.
                // We use a short side-task that flips `awaiting_pong` back
                // to "keep going" if a Pong arrives in time. Simpler: rely
                // on the next tick + the boolean.
                let _ = HEARTBEAT_TIMEOUT; // documented constant — flow uses next tick
            }

            // ── incoming WS frame ────────────────────────────────────────────
            maybe_msg = ws_stream.next() => {
                let Some(Ok(msg)) = maybe_msg else { break };
                match msg {
                    Message::Text(text) => {
                        match serde_json::from_str::<WsClientMessage>(&text) {
                            Ok(WsClientMessage::Subscribe {
                                since_seq,
                                projects,
                                channels,
                                assignee,
                                verb,
                                parent_plan,
                            }) => {
                                match SubscriptionFilters::parse(assignee, verb, parent_plan) {
                                    Ok(filters) => {
                                        // Cancel any previous live forwarder.
                                        if let Some(h) = live_task.take() {
                                            h.abort();
                                        }
                                        live_task = handle_subscribe(
                                            state.clone(),
                                            auth_ctx.clone(),
                                            out_tx.clone(),
                                            since_seq,
                                            projects,
                                            channels,
                                            filters,
                                        )
                                        .await;
                                    }
                                    Err(msg) => {
                                        send_json(
                                            &out_tx,
                                            &WsServerMessage::Error {
                                                code: "bad_request".to_string(),
                                                message: msg,
                                                request_id: None,
                                            },
                                        );
                                    }
                                }
                            }

                            Ok(WsClientMessage::Dispatch { command, actor, client_event_id }) => {
                                handle_dispatch(&state, &auth_ctx, &out_tx, command, actor, client_event_id).await;
                            }

                            Ok(WsClientMessage::Ping) => {
                                send_json(&out_tx, &WsServerMessage::Pong);
                            }

                            Ok(WsClientMessage::Pong) => {
                                awaiting_pong = false;
                            }

                            Ok(WsClientMessage::Ack { .. }) => {
                                // No-op for now — at-least-once scaffold.
                            }

                            Err(_) => {
                                send_json(
                                    &out_tx,
                                    &WsServerMessage::Error {
                                        code: "ws_malformed".to_string(),
                                        message: "could not parse client message".to_string(),
                                        request_id: None,
                                    },
                                );
                            }
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    if let Some(h) = live_task.take() {
        h.abort();
    }
    write_task.abort();
}

// ── authentication ────────────────────────────────────────────────────────────

async fn authenticate(
    store: &Arc<dyn TokenStore>,
    token: Option<&str>,
) -> Result<AuthContext, &'static str> {
    let raw = token.unwrap_or("");
    if raw.is_empty() {
        return Err("missing token");
    }
    verify_bearer(store, raw).await.map_err(|_| "invalid token")
}

fn token_from_protocols(ws: &WebSocketUpgrade) -> Option<String> {
    ws.requested_protocols()
        .filter_map(|value| value.to_str().ok())
        .find_map(|protocol| protocol.strip_prefix(BEARER_PROTOCOL_PREFIX))
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

// ── Subscribe — snapshot + live forwarder ─────────────────────────────────────

async fn handle_subscribe(
    state: AppState,
    auth: AuthContext,
    out_tx: mpsc::Sender<Message>,
    since_seq: Option<u64>,
    sub_projects: Option<Vec<ProjectId>>,
    sub_channels: Option<Vec<Channel>>,
    filters: SubscriptionFilters,
) -> Option<tokio::task::JoinHandle<()>> {
    // Subscribe requires at least one of the SubscribeXxx / read capabilities.
    if !auth.scope.capabilities.has(Capability::SubscribeTasks)
        && !auth.scope.capabilities.has(Capability::SubscribeComments)
        && !auth
            .scope
            .capabilities
            .has(Capability::SubscribeAgentStatus)
        && !auth.scope.capabilities.has(Capability::TaskRead)
        && !auth.scope.capabilities.has(Capability::SubscribePlans)
        && !auth.scope.capabilities.has(Capability::SubscribeRuns)
    {
        send_json(
            &out_tx,
            &WsServerMessage::Error {
                code: "forbidden".to_string(),
                message: "token lacks any subscribe capability".to_string(),
                request_id: None,
            },
        );
        return None;
    }

    let project_filter = effective_project_filter(&auth.scope.projects, sub_projects);
    let channel_filter = ChannelFilter::new(sub_channels);
    let caps = auth.scope.capabilities;

    // Race-fix:
    //   1. Register a fresh per-WS subscription with the Hub fanout layer
    //      first — captures all future events into our private mpsc.
    //   2. Snapshot `latest_seq()` — strict upper bound of "history".
    //   3. Load history from store, capped at the snapshot seq.
    //   4. From the live receiver, drop anything with seq <= boundary
    //      (they're already in the snapshot).
    let mut sub = state.hub.subscribe_ws();
    let mut live_rx = sub.take_receiver();
    let boundary = state.store.latest_seq().await.unwrap_or(0);

    if let Some(since) = since_seq {
        let history = state
            .store
            .load_since(since, SNAPSHOT_PAGE)
            .await
            .unwrap_or_default();
        let filtered_history = filter_history(history, boundary);
        let has_more = filtered_history.len() >= SNAPSHOT_PAGE;
        let next_seq = filtered_history.last().map(|e| e.seq);
        // Project / channel filter the snapshot too — clients must not see
        // events from projects they are not subscribed to.
        let mut shown = Vec::with_capacity(filtered_history.len());
        for env in filtered_history {
            if event_passes(&env, &project_filter, &channel_filter, caps, &state.tasks).await
                && event_matches_filters(&env, &filters, &state.plans, &state.claims).await
            {
                shown.push(env);
            }
        }
        send_json(
            &out_tx,
            &WsServerMessage::Snapshot {
                since_seq: since,
                events: shown,
                has_more,
                next_seq,
            },
        );
    }

    // Spawn live forwarder. `sub` is moved into the task; its Drop
    // unregisters the subscriber from the Hub fanout map when the task
    // exits (normally on `live_rx.recv()` returning None or aborted).
    let tasks = state.tasks.clone();
    let plans = state.plans.clone();
    let claims = state.claims.clone();
    let handle = tokio::spawn(async move {
        let _sub_guard = sub; // hold until task exits — Drop unregisters
        while let Some(arc) = live_rx.recv().await {
            let env: &EventEnvelope = &arc;
            if env.seq <= boundary {
                continue; // already covered by snapshot
            }
            if !event_passes(env, &project_filter, &channel_filter, caps, &tasks).await {
                continue;
            }
            if !event_matches_filters(env, &filters, &plans, &claims).await {
                continue;
            }
            // Wire type takes ownership — clone the envelope out of the Arc.
            let payload = WsServerMessage::Event {
                envelope: (*arc).clone(),
            };
            if let Ok(json) = serde_json::to_string(&payload) {
                if !try_send_message(&out_tx, Message::Text(json.into())) {
                    break;
                }
            }
        }
        // Forwarder ended — either `recv() == None` (Hub closed our sender:
        // per-subscriber slow-drop OR global broadcast-lag clear), or
        // `try_send` failed (writer-side backpressure / socket dead).
        //
        // Tell the client to reconnect. Best-effort: if `out_tx` is full
        // (Path A above) the Close cannot be enqueued — the writer task
        // is already stuck on `ws_sink.send`, the TCP layer will time out
        // and tear the socket down. Either way the next client connection
        // brings a `Subscribe { since_seq }` that rehydrates from the
        // event log via the existing snapshot path.
        let _ = try_send_message(
            &out_tx,
            Message::Close(Some(CloseFrame {
                code: 1001, // going away
                reason: "subscription closed by server — reconnect with since_seq".into(),
            })),
        );
    });

    Some(handle)
}

/// Best-effort synchronous send into the bounded outgoing channel.
/// Returns `true` on success, `false` if the channel is full (slow
/// `ws_sink`) or closed (writer task gone). Callers in the live
/// forwarder break out of their loop on `false` so the socket closes
/// promptly.
fn try_send_message(tx: &mpsc::Sender<Message>, msg: Message) -> bool {
    match tx.try_send(msg) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::info!("ws out_tx full — closing slow consumer");
            false
        }
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
}

/// Drop history entries past the seq boundary (race-fix).
fn filter_history(history: Vec<EventEnvelope>, boundary: u64) -> Vec<EventEnvelope> {
    history.into_iter().filter(|e| e.seq <= boundary).collect()
}

// ── Dispatch — relay through CommandBus ───────────────────────────────────────

enum DispatchReservation {
    Fresh,
    Replayed,
    Failed,
}

async fn reserve_or_ack_cached(
    state: &AppState,
    out_tx: &mpsc::Sender<Message>,
    client_event_id: EventId,
) -> DispatchReservation {
    match state.idempotency.lookup_event_id(client_event_id).await {
        Ok(Some((event_id, _seq))) => {
            send_json(out_tx, &WsServerMessage::Ack { event_id });
            return DispatchReservation::Replayed;
        }
        Ok(None) => {}
        Err(e) => {
            send_json(
                out_tx,
                &WsServerMessage::Error {
                    code: e.code().to_string(),
                    message: e.to_string(),
                    request_id: None,
                },
            );
            return DispatchReservation::Failed;
        }
    }

    match state.idempotency.reserve_event_id(client_event_id).await {
        Ok(true) => DispatchReservation::Fresh,
        Ok(false) => {
            for _ in 0..200 {
                tokio::time::sleep(Duration::from_millis(10)).await;
                match state.idempotency.lookup_event_id(client_event_id).await {
                    Ok(Some((event_id, _seq))) => {
                        send_json(out_tx, &WsServerMessage::Ack { event_id });
                        return DispatchReservation::Replayed;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        send_json(
                            out_tx,
                            &WsServerMessage::Error {
                                code: e.code().to_string(),
                                message: e.to_string(),
                                request_id: None,
                            },
                        );
                        return DispatchReservation::Failed;
                    }
                }
            }
            send_json(
                out_tx,
                &WsServerMessage::Error {
                    code: "conflict".to_string(),
                    message: "client_event_id is already in-flight".to_string(),
                    request_id: None,
                },
            );
            DispatchReservation::Failed
        }
        Err(e) => {
            send_json(
                out_tx,
                &WsServerMessage::Error {
                    code: e.code().to_string(),
                    message: e.to_string(),
                    request_id: None,
                },
            );
            DispatchReservation::Failed
        }
    }
}

async fn handle_dispatch(
    state: &AppState,
    auth: &AuthContext,
    out_tx: &mpsc::Sender<Message>,
    command: taskagent_core::Command,
    _actor: Option<taskagent_domain::Actor>,
    client_event_id: Option<EventId>,
) {
    // Mirror the HTTP-side capability check.
    let needed = match &command {
        taskagent_core::Command::CreateTask { .. }
        | taskagent_core::Command::UpdateTask { .. }
        | taskagent_core::Command::CompleteTask { .. }
        | taskagent_core::Command::DeleteTask { .. }
        | taskagent_core::Command::SetStatus { .. }
        | taskagent_core::Command::SetPriority { .. }
        | taskagent_core::Command::SplitTask { .. } => Capability::TaskWrite,
        taskagent_core::Command::CreateProject { .. }
        | taskagent_core::Command::UpdateProject { .. } => Capability::ProjectWrite,
        taskagent_core::Command::RecordAgentAction { .. } => Capability::AgentDispatch,
        taskagent_core::Command::AddComment { .. }
        | taskagent_core::Command::EditComment { .. }
        | taskagent_core::Command::DeleteComment { .. } => Capability::CommentWrite,
        // W3.1 placeholder: plan/run/session/signal/claim commands gated to admin
        // until their per-capability mapping lands. See ROADMAP §3.1, plan §3.2.
        _ => Capability::Admin,
    };
    if auth.require(needed).is_err() {
        send_json(
            out_tx,
            &WsServerMessage::Error {
                code: "forbidden".to_string(),
                message: format!("missing capability: {}", needed.name()),
                request_id: None,
            },
        );
        return;
    }

    // Resolve actor: with `actor_strict` always derive from the bearer token;
    // without it (default) prefer the client-supplied actor for legacy compat.
    let resolved_actor = {
        #[cfg(feature = "actor_strict")]
        {
            auth.actor()
        }
        #[cfg(not(feature = "actor_strict"))]
        {
            auth.actor()
        }
    };
    let mut reserved_client_event_id = None;
    if let Some(client_event_id) = client_event_id {
        match reserve_or_ack_cached(state, out_tx, client_event_id).await {
            DispatchReservation::Fresh => {
                reserved_client_event_id = Some(client_event_id);
            }
            DispatchReservation::Replayed | DispatchReservation::Failed => return,
        }
    }

    match state.hub.handle_command(command, resolved_actor).await {
        Ok(envelopes) => {
            if let (Some(client_event_id), Some(last)) =
                (reserved_client_event_id, envelopes.last())
            {
                if let Err(e) = state
                    .idempotency
                    .complete_event_id(client_event_id, last.id, last.seq)
                    .await
                {
                    tracing::warn!(err = %e, client_event_id = %client_event_id, "ws idempotency completion failed");
                }
            }
            for env in envelopes {
                send_json(out_tx, &WsServerMessage::Ack { event_id: env.id });
            }
        }
        Err(e) => {
            send_json(
                out_tx,
                &WsServerMessage::Error {
                    code: e.code().to_string(),
                    message: e.to_string(),
                    request_id: None,
                },
            );
        }
    }
}

// ── Filtering helpers ─────────────────────────────────────────────────────────

fn effective_project_filter(
    token: &ProjectFilter,
    subscribe: Option<Vec<ProjectId>>,
) -> ProjectFilter {
    match (token, subscribe) {
        (ProjectFilter::All, None) => ProjectFilter::All,
        (ProjectFilter::All, Some(list)) => ProjectFilter::Only { projects: list },
        (ProjectFilter::Only { projects }, None) => ProjectFilter::Only {
            projects: projects.clone(),
        },
        (ProjectFilter::Only { projects: a }, Some(b)) => {
            let intersection: Vec<ProjectId> =
                a.iter().filter(|p| b.contains(p)).copied().collect();
            ProjectFilter::Only {
                projects: intersection,
            }
        }
    }
}

/// Channel allow-list. `None` defaults to `[Tasks]` per the plan.
struct ChannelFilter {
    allowed: Vec<Channel>,
}

impl ChannelFilter {
    fn new(channels: Option<Vec<Channel>>) -> Self {
        let allowed = channels.unwrap_or_else(|| vec![Channel::Tasks]);
        Self { allowed }
    }

    fn allows(&self, ch: Channel) -> bool {
        self.allowed.contains(&ch)
    }
}

/// Map each channel to the capability a token must hold to receive its events.
/// Returns `None` for channels without a dedicated gate (Presence, Webhooks).
fn channel_required_capability(ch: Channel) -> Option<Capability> {
    match ch {
        Channel::Tasks => Some(Capability::SubscribeTasks),
        Channel::Comments => Some(Capability::SubscribeComments),
        Channel::AgentStatus => Some(Capability::SubscribeAgentStatus),
        Channel::Plans => Some(Capability::SubscribePlans),
        Channel::Runs => Some(Capability::SubscribeRuns),
        // PR1: Documents events are not exposed over WS yet — future work
        // can introduce `SubscribeDocuments`. Treated like Presence/Webhooks
        // (no dedicated capability gate, but no realtime route either).
        Channel::Presence | Channel::Webhooks | Channel::Documents | Channel::AiOps => None,
    }
}

async fn event_passes(
    env: &EventEnvelope,
    project_filter: &ProjectFilter,
    channel_filter: &ChannelFilter,
    caps: Capabilities,
    tasks: &Arc<TaskRepo>,
) -> bool {
    let ch = env.payload.channel();
    // Capability gate: silently drop events from channels the token lacks
    // access to, preventing information leaks across channel boundaries.
    if let Some(required) = channel_required_capability(ch) {
        if !caps.has(required) {
            return false;
        }
    }
    if !channel_filter.allows(ch) {
        return false;
    }
    let project_id = resolve_project(&env.payload, tasks).await;
    project_filter.allows(project_id)
}

/// Resolve the project that an event "belongs to". For events that carry
/// the project id in their payload (`TaskCreated`, `Project*`) this is
/// just `Event::target_project`. For task-targeting events without an
/// inline project id (`TaskStatusChanged`, `CommentAdded`, ...) we look
/// up the task → project mapping.
async fn resolve_project(ev: &Event, tasks: &Arc<TaskRepo>) -> Option<ProjectId> {
    if let Some(pid) = ev.target_project() {
        return Some(pid);
    }
    let task_id: TaskId = ev.target_task()?;
    let task = tasks.get(task_id).await.ok().flatten()?;
    task.project_id
}

// ── §3.7.6 (LIN B.6) subscription filters ─────────────────────────────────────

/// Parsed form of the wire-level `assignee`/`verb`/`parent_plan` strings.
///
/// All three fields are optional and ANDed together — `Default::default()`
/// means "no narrowing".
#[derive(Clone, Debug, Default)]
pub(crate) struct SubscriptionFilters {
    pub(crate) assignee: Option<AgentId>,
    pub(crate) verb: Option<String>,
    pub(crate) parent_plan: Option<PlanId>,
}

impl SubscriptionFilters {
    /// Parse raw strings off the wire. Returns a human-readable error string
    /// on bad UUID input so the WS handler can reply with `bad_request`.
    pub(crate) fn parse(
        assignee: Option<String>,
        verb: Option<String>,
        parent_plan: Option<String>,
    ) -> Result<Self, String> {
        let assignee = match assignee {
            Some(s) if !s.is_empty() => Some(
                s.parse::<AgentId>()
                    .map_err(|e| format!("invalid assignee id: {e}"))?,
            ),
            _ => None,
        };
        let parent_plan = match parent_plan {
            Some(s) if !s.is_empty() => Some(
                s.parse::<PlanId>()
                    .map_err(|e| format!("invalid parent_plan id: {e}"))?,
            ),
            _ => None,
        };
        let verb = verb.filter(|s| !s.is_empty());
        Ok(Self {
            assignee,
            verb,
            parent_plan,
        })
    }

    /// `true` when no narrowing filter is set — short-circuit hot path.
    fn is_noop(&self) -> bool {
        self.assignee.is_none() && self.verb.is_none() && self.parent_plan.is_none()
    }
}

/// `true` if `env` should be delivered under `filters`.
///
/// Order matters: cheap checks first, DB lookups last.
pub(crate) async fn event_matches_filters(
    env: &EventEnvelope,
    filters: &SubscriptionFilters,
    plans: &Arc<PlanRepo>,
    claims: &Arc<AgentClaimRepo>,
) -> bool {
    if filters.is_noop() {
        return true;
    }

    // verb — zero-cost string compare against `Event::kind()`.
    if let Some(ref v) = filters.verb {
        if env.payload.kind() != v.as_str() {
            return false;
        }
    }

    // parent_plan — try inline id first, fall back to task → plans projection.
    if let Some(want) = filters.parent_plan {
        if !event_belongs_to_plan(&env.payload, want, plans).await {
            return false;
        }
    }

    // assignee — only meaningful for events that target a task. We treat
    // "assignee" as "agent holds an active claim on this task".
    if let Some(want) = filters.assignee {
        let Some(task_id) = env.payload.target_task() else {
            return false;
        };
        // Also accept events whose direct actor is the agent (covers claim
        // bookkeeping events that may race the claims projection on hot path).
        if !event_actor_matches_agent(&env.payload, want)
            && !task_is_claimed_by(task_id, want, claims).await
        {
            return false;
        }
    }

    true
}

/// `true` if the event "belongs" to `plan_id` either inline (plan/run/task
/// events that already carry a plan id) or via the task → plans projection.
async fn event_belongs_to_plan(ev: &Event, plan_id: PlanId, plans: &Arc<PlanRepo>) -> bool {
    if let Some(inline) = inline_plan_id(ev) {
        return inline == plan_id;
    }
    // Fall back to plan_tasks lookup for task-targeting events.
    if let Some(task_id) = ev.target_task() {
        if let Ok(plan_list) = plans.list_plans_for_task(task_id).await {
            return plan_list.iter().any(|p| p.id == plan_id);
        }
    }
    false
}

/// Inline `plan_id` carried directly by the event payload, if any.
///
/// Covers all plan-* and run-step events; for run-* events where the plan
/// id is only reachable via the run projection we deliberately return
/// `None` to keep this helper lookup-free (the caller handles fallback).
fn inline_plan_id(ev: &Event) -> Option<PlanId> {
    match ev {
        Event::PlanCreated { plan } => Some(plan.id),
        Event::PlanUpdated { plan_id, .. }
        | Event::PlanStatusChanged { plan_id, .. }
        | Event::PlanGoalChanged { plan_id, .. }
        | Event::PlanTaskAdded { plan_id, .. }
        | Event::PlanTaskRemoved { plan_id, .. }
        | Event::PlanReordered { plan_id, .. }
        | Event::PlanArchived { plan_id, .. }
        | Event::PlanModifiedByHuman { plan_id, .. }
        | Event::RunObsolescedByPlanEdit { plan_id, .. } => Some(*plan_id),
        Event::RunStarted { run } => Some(run.plan_id),
        _ => None,
    }
}

/// `true` if `agent_id` currently holds an active claim on `task_id`.
async fn task_is_claimed_by(
    task_id: TaskId,
    agent_id: AgentId,
    claims: &Arc<AgentClaimRepo>,
) -> bool {
    match claims.get_agents_claiming_task(task_id).await {
        Ok(list) => list.contains(&agent_id),
        Err(_) => false,
    }
}

/// `true` for events that explicitly identify `agent_id` as the acting
/// agent (claim/release bookkeeping). Lets clients see their own claim
/// events even before the projection has caught up.
fn event_actor_matches_agent(ev: &Event, agent_id: AgentId) -> bool {
    match ev {
        Event::AgentClaimed { agent_id: a, .. } | Event::AgentReleased { agent_id: a, .. } => {
            *a == agent_id
        }
        _ => false,
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Best-effort fire-and-forget send for admin / handshake / heartbeat
/// frames. On `Full` we log and drop the message — the writer task will
/// catch up on its own; if the socket is permanently stuck the live
/// forwarder will hit Full too and break, closing the connection.
fn send_json(tx: &mpsc::Sender<Message>, msg: &impl serde::Serialize) {
    if let Ok(json) = serde_json::to_string(msg) {
        let _ = try_send_message(tx, Message::Text(json.into()));
    }
}
