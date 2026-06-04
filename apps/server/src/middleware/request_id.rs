//! Request-ID middleware — tags every request with a stable UUIDv7 identifier.
//!
//! ## Behaviour
//! 1. Reads the incoming `X-Request-Id` header.
//! 2. If absent, generates `req_<uuidv7>`.
//! 3. Inserts [`RequestId`] into request extensions (accessible by downstream handlers).
//! 4. Copies the same value into the `X-Request-Id` **response** header.

use axum::{extract::Request, http::HeaderValue, middleware::Next, response::Response};
use serde::{Deserialize, Serialize};

/// Newtype wrapper for the per-request identifier string.
///
/// Inserted into request extensions by [`request_id_middleware`] so that any
/// handler (or error formatter) can extract it via
/// `req.extensions().get::<RequestId>()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestId(pub String);

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Axum `from_fn`-compatible middleware function.
///
/// # Precedence
/// * If `X-Request-Id` is already present and contains a valid ASCII string it
///   is preserved as-is (pass-through).
/// * Otherwise a fresh `req_<uuidv7>` string is generated.
pub async fn request_id_middleware(mut req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .unwrap_or_else(|| format!("req_{}", uuid::Uuid::now_v7()));

    req.extensions_mut().insert(RequestId(id.clone()));

    let mut res = next.run(req).await;

    if let Ok(val) = HeaderValue::from_str(&id) {
        res.headers_mut().insert("x-request-id", val);
    }

    res
}
