//! Bearer-token authentication middleware.
//!
//! Parses `Authorization: Bearer <token>`, looks the token up via
//! [`daruma_auth::TokenStore`], inserts an [`AuthContext`] into the
//! request extensions on success, returns a structured 401 on failure.
//!
//! The middleware is **not** applied to `/v1/healthz` or `/v1/ws`
//! (routes.rs keeps them in a separate public router). WS auth happens
//! through the `Sec-WebSocket-Protocol` subprotocol (Wave 2 / W2.3).
//!
//! ## Token verification cache
//!
//! `verify_bearer` runs argon2id on each call (~50 ms). Without caching
//! every authenticated request pays that cost end-to-end. We memoise
//! verified bearer strings for [`CACHE_TTL`] (30 s) keyed by SHA-256 of the
//! bearer so the in-memory map never stores plaintext tokens. Revocation
//! lag is bounded by the TTL.
//!
//! Capacity is bounded by [`CACHE_MAX_ENTRIES`]; once exceeded, expired
//! entries are pruned opportunistically on the next miss.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use daruma_auth::{verify_bearer, AuthContext, TokenStore, VerifyError};

/// How long a successfully verified bearer stays in the in-memory cache.
const CACHE_TTL: Duration = Duration::from_secs(30);

/// Soft cap on cached entries before opportunistic pruning kicks in.
const CACHE_MAX_ENTRIES: usize = 2_048;

type CacheKey = [u8; 32];

#[derive(Clone)]
struct CacheEntry {
    ctx: AuthContext,
    expires_at: Instant,
}

/// State injected into [`require_auth`] via `from_fn_with_state`.
#[derive(Clone)]
pub struct AuthLayer {
    pub store: Arc<dyn TokenStore>,
    cache: Arc<Mutex<HashMap<CacheKey, CacheEntry>>>,
}

impl AuthLayer {
    pub fn new(store: Arc<dyn TokenStore>) -> Self {
        Self {
            store,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

fn hash_bearer(bearer: &str) -> CacheKey {
    let mut hasher = Sha256::new();
    hasher.update(bearer.as_bytes());
    hasher.finalize().into()
}

fn cache_lookup(layer: &AuthLayer, key: &CacheKey) -> Option<AuthContext> {
    let mut cache = layer.cache.lock().ok()?;
    match cache.get(key) {
        Some(entry) if Instant::now() < entry.expires_at => Some(entry.ctx.clone()),
        Some(_) => {
            cache.remove(key);
            None
        }
        None => None,
    }
}

fn cache_store(layer: &AuthLayer, key: CacheKey, ctx: AuthContext) {
    let Ok(mut cache) = layer.cache.lock() else {
        return;
    };
    if cache.len() >= CACHE_MAX_ENTRIES {
        let now = Instant::now();
        cache.retain(|_, entry| now < entry.expires_at);
    }
    cache.insert(
        key,
        CacheEntry {
            ctx,
            expires_at: Instant::now() + CACHE_TTL,
        },
    );
}

/// Axum `from_fn_with_state` compatible middleware function.
pub async fn require_auth(
    State(layer): State<AuthLayer>,
    mut req: Request,
    next: Next,
) -> Response {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Empty headers skip the cache and fall straight through to the
    // verifier (it will return a structured 401).
    if header.is_empty() {
        match verify_bearer(&layer.store, header).await {
            Ok(ctx) => {
                insert_auth_context(&mut req, ctx);
                return next.run(req).await;
            }
            Err(err) => return render_auth_error(err, &req),
        }
    }

    let key = hash_bearer(header);
    if let Some(ctx) = cache_lookup(&layer, &key) {
        insert_auth_context(&mut req, ctx);
        return next.run(req).await;
    }

    match verify_bearer(&layer.store, header).await {
        Ok(ctx) => {
            cache_store(&layer, key, ctx.clone());
            insert_auth_context(&mut req, ctx);
            next.run(req).await
        }
        Err(err) => render_auth_error(err, &req),
    }
}

fn insert_auth_context(req: &mut Request, ctx: AuthContext) {
    req.extensions_mut().insert(ctx);
}

fn render_auth_error(err: VerifyError, req: &Request) -> Response {
    let request_id = req
        .extensions()
        .get::<crate::middleware::request_id::RequestId>()
        .map(|r| r.0.clone());

    let body = json!({
        "error": {
            "code": err.code(),
            "message": err.message(),
            "request_id": request_id,
        }
    });
    (StatusCode::UNAUTHORIZED, Json(body)).into_response()
}
