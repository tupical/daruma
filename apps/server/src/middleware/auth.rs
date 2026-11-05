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
//! bearer so the in-memory map never stores plaintext tokens.
//!
//! A revoke through this server's `DELETE /v1/tokens/{id}` latches the token
//! in the [`Hub`] before it answers, and the latch outranks the cache on every
//! lookup. So a later request gets `401`, also when a verification that read
//! the row before the revoke fills the cache after it. A request that already
//! passed this middleware finishes.
//!
//! A revoke that skips the hub (another process, a direct `TokenRepo` write)
//! is seen only by the next verification, so a cached bearer keeps working
//! until its entry expires: [`CACHE_TTL`] counted from the fill, which may
//! follow the revoke, not from the revoke.
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
use daruma_auth::{
    sanitize_client, verify_bearer, AuthContext, HostPrincipal, TokenStore, VerifyError,
    CLIENT_HEADER,
};
use daruma_sync::Hub;
use serde_json::json;
use sha2::{Digest, Sha256};

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
    hub: Arc<Hub>,
    cache: Arc<Mutex<HashMap<CacheKey, CacheEntry>>>,
}

impl AuthLayer {
    pub fn new(store: Arc<dyn TokenStore>, hub: Arc<Hub>) -> Self {
        Self {
            store,
            hub,
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
        Some(entry)
            if Instant::now() < entry.expires_at
                && !layer.hub.is_token_revoked(entry.ctx.token_id) =>
        {
            Some(entry.ctx.clone())
        }
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
        // The row verified above may predate a revoke that returned since.
        Ok(ctx) if layer.hub.is_token_revoked(ctx.token_id) => {
            render_auth_error(VerifyError::Revoked, &req)
        }
        Ok(ctx) => {
            if ctx.device_id.is_none() {
                cache_store(&layer, key, ctx.clone());
            }
            insert_auth_context(&mut req, ctx);
            next.run(req).await
        }
        Err(err) => render_auth_error(err, &req),
    }
}

fn insert_auth_context(req: &mut Request, mut ctx: AuthContext) {
    if let Some(HostPrincipal(id)) = req.extensions().get::<HostPrincipal>().copied() {
        ctx.principal_id = Some(id);
    }
    // Per request: the cached context must not carry a previous call's client.
    ctx.client = req
        .headers()
        .get(CLIENT_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(sanitize_client);
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

#[cfg(test)]
mod tests {
    use super::*;

    use axum::{body::Body, middleware::from_fn_with_state, routing::get, Router};
    use daruma_auth::{generate, NewTokenSpec, TokenKind, TokenScope};
    use daruma_core::{CommandBus, CommandHandler};
    use daruma_events::{EventBus, EventStore};
    use daruma_shared::AgentId;
    use daruma_storage::{
        ActivityRepo, CommentRepo, Db, ProjectRepo, SqliteEventStore, TaskRepo, TokenRepo,
    };
    use tower::ServiceExt;

    struct Fixture {
        layer: AuthLayer,
        store: Arc<dyn TokenStore>,
        token_id: daruma_shared::TokenId,
        bearer: String,
    }

    async fn fixture() -> Fixture {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let pool = db.pool().clone();
        let events: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
        let bus = EventBus::new(16);
        let handler = CommandHandler::new(
            events,
            Arc::new(TaskRepo::new(pool.clone())),
            Arc::new(ProjectRepo::new(pool.clone())),
            Arc::new(CommentRepo::new(pool.clone())),
            Arc::new(ActivityRepo::new(pool.clone())),
            bus.clone(),
        );
        let hub = Arc::new(Hub::new(bus, Arc::new(CommandBus::new(Arc::new(handler)))));
        let store: Arc<dyn TokenStore> = Arc::new(TokenRepo::new(pool));
        let secret = generate(NewTokenSpec {
            kind: TokenKind::Pat,
            agent_id: AgentId::new(),
            scope: TokenScope::default_user(),
            rate_limit_per_min: 60,
            expired_at: None,
        })
        .unwrap();
        let token_id = secret.record.id;
        store.insert(secret.record).await.unwrap();
        Fixture {
            layer: AuthLayer::new(store.clone(), hub),
            store,
            token_id,
            bearer: format!("Bearer {}", secret.plaintext),
        }
    }

    async fn status(layer: &AuthLayer, bearer: &str) -> StatusCode {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(from_fn_with_state(layer.clone(), require_auth));
        let request = Request::builder()
            .uri("/")
            .header("authorization", bearer)
            .body(Body::empty())
            .unwrap();
        app.oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn expired_entry_lets_verification_see_a_revoke_the_hub_missed() {
        let f = fixture().await;
        assert_eq!(status(&f.layer, &f.bearer).await, StatusCode::OK);

        f.store.revoke(f.token_id).await.unwrap();
        assert_eq!(status(&f.layer, &f.bearer).await, StatusCode::OK);

        for entry in f.layer.cache.lock().unwrap().values_mut() {
            entry.expires_at = Instant::now();
        }
        assert_eq!(status(&f.layer, &f.bearer).await, StatusCode::UNAUTHORIZED);
    }

    /// The worst order for a concurrent fill: it lands after the revoke
    /// returned, past any check the verification itself could make.
    #[tokio::test]
    async fn entry_filled_after_the_latch_is_never_served() {
        let f = fixture().await;
        let ctx = verify_bearer(&f.store, &f.bearer).await.unwrap();

        f.store.revoke(f.token_id).await.unwrap();
        f.layer.hub.notify_token_revoked(f.token_id);
        cache_store(&f.layer, hash_bearer(&f.bearer), ctx);

        assert_eq!(status(&f.layer, &f.bearer).await, StatusCode::UNAUTHORIZED);
        assert!(f.layer.cache.lock().unwrap().is_empty());
    }
}
