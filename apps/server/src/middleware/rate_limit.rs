//! Per-tenant/per-token rate limiting for authenticated HTTP routes.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use taskagent_auth::AuthContext;

const DEFAULT_TENANT_KEY: &str = "self-hosted";

#[derive(Clone, Default)]
pub struct RateLimiter {
    buckets: Arc<Mutex<HashMap<RateLimitKey, Bucket>>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RateLimitKey {
    tenant_id: String,
    token_id: String,
}

#[derive(Clone, Debug)]
struct Bucket {
    tokens: f64,
    updated_at: Instant,
}

impl RateLimiter {
    pub fn check(&self, ctx: &AuthContext) -> Result<(), Duration> {
        let limit = ctx.rate_limit_per_min.max(1);
        let capacity = limit as f64;
        let refill_per_sec = capacity / 60.0;
        let key = RateLimitKey {
            tenant_id: ctx
                .tenant_id
                .clone()
                .unwrap_or_else(|| DEFAULT_TENANT_KEY.to_string()),
            token_id: ctx.token_id.to_string(),
        };
        let now = Instant::now();

        let Ok(mut buckets) = self.buckets.lock() else {
            return Ok(());
        };
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: capacity,
            updated_at: now,
        });
        let elapsed = now.duration_since(bucket.updated_at).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill_per_sec).min(capacity);
        bucket.updated_at = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }

        let missing = 1.0 - bucket.tokens;
        let retry_after = (missing / refill_per_sec).ceil().max(1.0) as u64;
        Err(Duration::from_secs(retry_after))
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
        Err(retry_after) => {
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
    }
}
