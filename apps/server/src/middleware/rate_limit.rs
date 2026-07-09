//! Per-tenant/per-token rate limiting for authenticated HTTP routes,
//! and IP-keyed rate limiting for unauthenticated pairing endpoints.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use daruma_auth::AuthContext;
use serde_json::json;

const DEFAULT_TENANT_KEY: &str = "self-hosted";

/// Maximum pairing attempts per source IP per minute.
const PAIRING_LIMIT_PER_MIN: u32 = 5;

#[derive(Clone, Default)]
pub struct RateLimiter {
    buckets: Arc<Mutex<HashMap<RateLimitKey, Bucket>>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum RateLimitKey {
    Token { tenant_id: String, token_id: String },
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
            tenant_id: ctx
                .tenant_id
                .clone()
                .unwrap_or_else(|| DEFAULT_TENANT_KEY.to_string()),
            token_id: ctx.token_id.to_string(),
        };
        self.check_key(key, limit)
    }

    /// Check the IP-keyed bucket used for unauthenticated pairing requests.
    pub fn check_pairing_ip(&self, ip: &str) -> Result<(), Duration> {
        let key = RateLimitKey::Ip(ip.to_string());
        self.check_key(key, PAIRING_LIMIT_PER_MIN as f64)
    }
}

pub async fn enforce_rate_limit(
    State(limiter): State<RateLimiter>,
    req: Request,
    next: Next,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>() else {
        return next.run(req).await;
    };

    match limiter.check(ctx) {
        Ok(()) => next.run(req).await,
        Err(retry_after) => rate_limited_response(retry_after),
    }
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

fn rate_limited_response(retry_after: Duration) -> Response {
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
