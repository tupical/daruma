//! CORS layer — very permissive for MVP/development.

use tower_http::cors::CorsLayer;

/// Returns a permissive [`CorsLayer`] suitable for local development.
pub fn cors_layer() -> CorsLayer {
    CorsLayer::very_permissive()
}
