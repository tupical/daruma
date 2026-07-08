//! WebSocket wire types for `/v1/ws`.
//!
//! Previously defined in `crates/sync/src/wire.rs`. Moved here so the WASM
//! frontend can import them without pulling in `daruma-sync`'s tokio
//! runtime dependency.

use daruma_domain::Actor;
use daruma_events::{Channel, EventEnvelope};
use daruma_shared::{EventId, ProjectId};
use serde::{Deserialize, Serialize};

use crate::command::Command;

// ── Server → Client ───────────────────────────────────────────────────────────

/// Messages the server sends to each connected client.
// The `Event` variant intentionally carries the full `EventEnvelope` so clients
// receive the complete payload over the wire; boxing would only add an allocation
// without reducing the serialised size.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsServerMessage {
    /// First frame after a successful WS handshake. Carries the current
    /// `server_seq` so clients can decide whether to subscribe with a
    /// catch-up or only listen forward.
    Hello {
        server_seq: u64,
        capabilities: Vec<String>,
    },
    /// A new event was committed to the log.
    Event { envelope: EventEnvelope },
    /// Acknowledge receipt of a command; carries the first produced event id.
    Ack { event_id: EventId },
    /// Server-side error processing a client message (structured shape).
    Error {
        code: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// Historical replay of events in `since_seq < seq <= next_seq`.
    Snapshot {
        since_seq: u64,
        events: Vec<EventEnvelope>,
        has_more: bool,
        next_seq: Option<u64>,
    },
    /// The broadcast receiver lagged and the server had to drop frames.
    /// Clients reconcile by issuing a new `Subscribe { since_seq: from_seq }`.
    ///
    /// **Deprecated since §3.9.4 (2026-05-20):** replaced by per-subscriber
    /// `mpsc` backpressure in the WS Hub. A slow consumer now has its socket
    /// closed by the server; the client reconnects with `since_seq` and
    /// rehydrates via the `Snapshot` path. Kept on the wire for backward
    /// compatibility — no server code path emits this variant after §3.9.4.
    /// See `.omc/plans/3.9.4-ws-hub-mpsc.md`.
    Resync { from_seq: u64, dropped: u64 },
    /// Heartbeat — server-initiated ping (clients should reply with `Pong`).
    Ping,
    /// Heartbeat — server response to a client `Ping`.
    Pong,
}

// ── Client → Server ───────────────────────────────────────────────────────────

/// Messages a client sends to the server.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // Command grew with plan/run/session variants (W2.2); boxing adds no value here
pub enum WsClientMessage {
    /// Submit a command for processing.
    Dispatch {
        command: Command,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<Actor>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_event_id: Option<EventId>,
    },
    /// Subscribe to the live event stream with optional history catch-up
    /// and per-project / per-channel filtering.
    ///
    /// * `since_seq = None` — subscribe forward-only.
    /// * `since_seq = Some(s)` — first deliver a `Snapshot` covering
    ///   `(s, server_seq]`, then continue with live events.
    /// * `projects = None` — every project the token has access to.
    /// * `channels = None` — defaults to `[Channel::Tasks]`.
    ///
    /// §3.7.6 (LIN B.6) — optional narrowing filters, ANDed together with
    /// the project/channel filters above:
    /// * `assignee` — only events whose target task is actively claimed by
    ///   this agent id (string form of `AgentId`).
    /// * `verb` — only events whose `payload.type` (i.e. `Event::kind()`)
    ///   equals this string (e.g. `"task_status_changed"`).
    /// * `parent_plan` — only events tied to this `PlanId` (either inline
    ///   on the payload or via the task → plans projection).
    ///
    /// Backward-compat: clients that omit these fields keep the pre-§3.7.6
    /// fan-out behaviour.
    Subscribe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since_seq: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        projects: Option<Vec<ProjectId>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channels: Option<Vec<Channel>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assignee: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verb: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_plan: Option<String>,
    },
    /// Client-initiated ping (server replies with `Pong`).
    Ping,
    /// Client response to a server-initiated `Ping`.
    Pong,
    /// Ack a delivered event (at-least-once scaffold for future use).
    Ack { event_id: EventId },
}
