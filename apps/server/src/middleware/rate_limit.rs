//! Per-tenant/per-token rate limiting for authenticated HTTP routes,
//! and IP-keyed rate limiting for unauthenticated pairing endpoints.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    body::{Body, HttpBody},
    extract::{ConnectInfo, Request, State},
    http::{HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use daruma_auth::AuthContext;
use serde_json::json;

const DEFAULT_TENANT_KEY: &str = "self-hosted";

/// Maximum pairing attempts per source IP per minute.
const PAIRING_LIMIT_PER_MIN: u32 = 5;

/// Largest MCP body inspected to tell a handshake from a tool call. Handshake
/// frames are a few hundred bytes; anything bigger is charged as a call.
const MCP_CLASSIFY_MAX_BYTES: u64 = 64 * 1024;

/// Handshake frames a session-per-call client sends per tool call
/// (initialize, server/discover, notifications/initialized). The handshake
/// bucket holds this many times the token's limit, so the handshake never
/// becomes the bottleneck of such a client while staying bounded.
const MCP_HANDSHAKE_FRAMES_PER_CALL: f64 = 3.0;

/// JSON-RPC methods of the MCP session handshake. They never touch tenant
/// data, so they draw from their own per-token bucket instead of the budget
/// that `tools/call` and REST requests share: clients that open a session per
/// call (initialize + server/discover + notifications/initialized) must not
/// starve their own tool calls.
fn is_mcp_handshake_method(method: &str) -> bool {
    matches!(method, "initialize" | "ping" | "server/discover")
        || method.starts_with("notifications/")
}

#[derive(Clone, Default)]
pub struct RateLimiter {
    buckets: Arc<Mutex<HashMap<RateLimitKey, Bucket>>>,
}

/// In-process marker for a request whose token bucket was already charged.
/// HTTP clients cannot set request extensions.
#[derive(Clone, Copy, Debug)]
pub struct RateLimitAlreadyCharged;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum RateLimitKey {
    Token {
        tenant_id: String,
        token_id: String,
    },
    /// Separate per-token bucket for MCP handshake frames.
    McpHandshake {
        tenant_id: String,
        token_id: String,
    },
    Ip(String),
}

#[derive(Clone, Debug)]
struct Bucket {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    updated_at: Instant,
}

impl RateLimiter {
    fn check_key(&self, key: RateLimitKey, capacity: f64) -> Result<(), Duration> {
        let refill_per_sec = capacity / 60.0;
        let now = Instant::now();

        let Ok(mut buckets) = self.buckets.lock() else {
            return Ok(());
        };
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: capacity,
            capacity,
            refill_per_sec,
            updated_at: now,
        });
        let elapsed = now.duration_since(bucket.updated_at).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * bucket.refill_per_sec).min(bucket.capacity);
        bucket.updated_at = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }

        let missing = 1.0 - bucket.tokens;
        let retry_after = (missing / bucket.refill_per_sec).ceil().max(1.0) as u64;
        Err(Duration::from_secs(retry_after))
    }

    pub fn check(&self, ctx: &AuthContext) -> Result<(), Duration> {
        let limit = ctx.rate_limit_per_min.max(1) as f64;
        let key = RateLimitKey::Token {
            tenant_id: tenant_key(ctx),
            token_id: ctx.token_id.to_string(),
        };
        self.check_key(key, limit)
    }

    /// Charge an MCP handshake frame (see [`is_mcp_handshake_method`]) to the
    /// token's handshake bucket; the main bucket of [`Self::check`] is untouched.
    pub fn check_mcp_handshake(&self, ctx: &AuthContext) -> Result<(), Duration> {
        let limit = ctx.rate_limit_per_min.max(1) as f64 * MCP_HANDSHAKE_FRAMES_PER_CALL;
        let key = RateLimitKey::McpHandshake {
            tenant_id: tenant_key(ctx),
            token_id: ctx.token_id.to_string(),
        };
        self.check_key(key, limit)
    }

    /// Check the IP-keyed bucket used for unauthenticated pairing requests.
    pub fn check_pairing_ip(&self, ip: &str) -> Result<(), Duration> {
        let key = RateLimitKey::Ip(ip.to_string());
        self.check_key(key, PAIRING_LIMIT_PER_MIN as f64)
    }

    /// Check an IP-keyed bucket with a caller-chosen per-minute limit, for
    /// embedders guarding their own unauthenticated surfaces. Use a dedicated
    /// `RateLimiter` per surface: IP buckets of one instance are shared.
    pub fn check_ip(&self, ip: &str, limit_per_min: u32) -> Result<(), Duration> {
        self.check_key(
            RateLimitKey::Ip(ip.to_string()),
            limit_per_min.max(1) as f64,
        )
    }
}

fn tenant_key(ctx: &AuthContext) -> String {
    ctx.tenant_id
        .clone()
        .unwrap_or_else(|| DEFAULT_TENANT_KEY.to_string())
}

pub async fn enforce_rate_limit(
    State(limiter): State<RateLimiter>,
    req: Request,
    next: Next,
) -> Response {
    if req.extensions().get::<RateLimitAlreadyCharged>().is_some() {
        return next.run(req).await;
    }
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return next.run(req).await;
    };

    let (req, handshake) = classify_mcp_handshake(req).await;
    let charged = if handshake {
        limiter.check_mcp_handshake(&ctx)
    } else {
        limiter.check(&ctx)
    };
    match charged {
        Ok(()) => next.run(req).await,
        Err(retry_after) => rate_limited_response(retry_after),
    }
}

/// `true` when `req` is a POST to the MCP endpoint whose JSON-RPC message (or
/// every message of a batch) is a handshake method. The body is buffered only
/// when its size is known and small; the request is rebuilt unchanged.
async fn classify_mcp_handshake(req: Request) -> (Request, bool) {
    if req.method() != Method::POST || !req.uri().path().ends_with("/mcp") {
        return (req, false);
    }
    if !req
        .body()
        .size_hint()
        .upper()
        .is_some_and(|upper| upper <= MCP_CLASSIFY_MAX_BYTES)
    {
        return (req, false);
    }
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MCP_CLASSIFY_MAX_BYTES as usize).await else {
        // The body stream failed; the handler would fail the same way.
        return (Request::from_parts(parts, Body::empty()), false);
    };
    let handshake = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .is_some_and(|message| {
            let messages = match &message {
                serde_json::Value::Array(items) => items.as_slice(),
                single => std::slice::from_ref(single),
            };
            !messages.is_empty()
                && messages.iter().all(|message| {
                    message
                        .get("method")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(is_mcp_handshake_method)
                })
        });
    (Request::from_parts(parts, Body::from(bytes)), handshake)
}

/// Middleware for unauthenticated pairing endpoint — 5 req/min per source IP.
pub async fn enforce_pairing_rate_limit(
    State(limiter): State<RateLimiter>,
    req: Request,
    next: Next,
) -> Response {
    // Extract the peer IP from ConnectInfo if available, fall back to a fixed key.
    let ip = req
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    match limiter.check_pairing_ip(&ip) {
        Ok(()) => next.run(req).await,
        Err(retry_after) => rate_limited_response(retry_after),
    }
}

pub fn rate_limited_response(retry_after: Duration) -> Response {
    let body = json!({
        "error": {
            "code": "rate_limited",
            "message": "rate limit exceeded",
        }
    });
    let mut response = (StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response();
    let seconds = retry_after.as_secs().max(1).to_string();
    if let Ok(value) = HeaderValue::from_str(&seconds) {
        response.headers_mut().insert("retry-after", value);
    }
    response
}
