//! `POST /v1/devices/pair` — LAN pairing endpoint (§3.3.5).
//!
//! A client that obtained a pairing URL (via QR code or direct paste) POSTs
//! its fingerprint here. The server:
//!
//! 1. Consumes the single-use token from the in-process [`PairingStore`].
//! 2. Verifies the TLS fingerprint claimed by the client matches what the
//!    server actually presents (fingerprint mismatch → 403).
//! 3. Issues a new long-lived bearer token and returns it.
//!
//! The endpoint is intentionally unauthenticated (the pairing token itself
//! is the credential). It is protected by an IP-keyed rate limiter (5 req/min
//! per source IP) applied in [`crate::routes::public_routes`].
//!
//! ## Security properties
//!
//! - Single-use tokens prevent replay.
//! - TTL is enforced by [`PairingStore::consume`].
//! - Fingerprint verification ensures the client is actually talking to the
//!   same server that issued the QR.
//! - No secret material is logged.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
#[allow(unused_imports)]
use chrono;
use serde::{Deserialize, Serialize};
use taskagent_auth::{generate, NewTokenSpec, TokenKind, TokenScope};
use taskagent_shared::AgentId; // for Duration::days in token expiry

use crate::{error::ApiError, state::AppState};

// ── Request / response types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PairRequest {
    /// The single-use token from the `taskagent://pair?token=…` URL.
    pub token: String,
    /// The TLS fingerprint the client observed while connecting, e.g.
    /// `"sha256:ab12cd…"`.  Must match the server's actual certificate.
    pub tls_fingerprint: String,
    /// Optional human-readable device label (e.g. "Alice's MacBook").
    #[serde(default)]
    pub device_label: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PairResponse {
    /// Bearer token the new device should use for subsequent requests.
    pub access_token: String,
    /// Stable API prefix of the issued token (safe to log / display).
    pub token_prefix: String,
    /// The server's canonical base URL for future requests.
    pub server_url: String,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// `POST /v1/devices/pair`
///
/// Consumes the pairing token, verifies fingerprint, and issues a bearer token.
pub async fn pair_device(
    State(state): State<AppState>,
    Json(body): Json<PairRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // 1. Reject obviously-bad inputs before touching the store.
    let candidate_token = body.token.trim();
    if candidate_token.is_empty() {
        return Err(ApiError::from(taskagent_shared::CoreError::validation(
            "token must not be empty",
        )));
    }
    let claimed_fpr = body.tls_fingerprint.trim();
    if claimed_fpr.is_empty() {
        return Err(ApiError::from(taskagent_shared::CoreError::validation(
            "tls_fingerprint must not be empty",
        )));
    }

    // 2. Consume the single-use pairing ticket (constant-time; also sweeps
    //    expired tickets as a side-effect).
    let ticket = state
        .pairing
        .consume(candidate_token)
        .await
        .ok_or_else(|| {
            ApiError::from(taskagent_shared::CoreError::unauthorized(
                "invalid, expired, or already-used pairing token",
            ))
        })?;

    // 3. Verify TLS fingerprint (the client must prefix it with "sha256:").
    let expected = format!(
        "sha256:{}",
        ticket.tls_fingerprint.trim_start_matches("sha256:")
    );
    let provided = claimed_fpr.to_string();
    if !constant_eq_str(&expected, &provided) {
        tracing::warn!(
            device_label = body.device_label.as_deref().unwrap_or("<none>"),
            "pairing rejected: TLS fingerprint mismatch"
        );
        return Err(ApiError::status(
            StatusCode::FORBIDDEN,
            "TLS fingerprint mismatch — ensure you are connecting to the correct server",
        ));
    }

    // 4. Issue a new bearer token for the paired device.
    let label = body
        .device_label
        .as_deref()
        .unwrap_or("paired-device")
        .chars()
        .take(80)
        .collect::<String>();

    // Paired-device tokens expire in 90 days — bounded lifetime limits blast
    // radius if a token is ever compromised.
    let expires = taskagent_shared::time::now() + chrono::Duration::days(90);
    let secret = generate(NewTokenSpec {
        kind: TokenKind::Pat,
        agent_id: AgentId::new(),
        scope: TokenScope::default_user(),
        rate_limit_per_min: 120,
        expired_at: Some(expires),
    })
    .map_err(|e| ApiError::from(taskagent_shared::CoreError::storage(e.to_string())))?;

    state
        .auth_store
        .insert(secret.record.clone())
        .await
        .map_err(|e| ApiError::from(taskagent_shared::CoreError::storage(e.to_string())))?;

    tracing::info!(
        device = label,
        token_prefix = %secret.record.prefix,
        "device paired successfully"
    );

    Ok(Json(PairResponse {
        access_token: secret.plaintext,
        token_prefix: secret.record.prefix,
        server_url: format!("https://{}", ticket.host),
    }))
}

/// `GET /v1/devices/pair/ticket` — issue a new pairing ticket and return its
/// QR code PNG.  Requires an authenticated admin token so only the server
/// operator can initiate pairing.
pub async fn issue_pairing_ticket(
    auth: axum::Extension<taskagent_auth::AuthContext>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    use taskagent_auth::Capability;
    auth.require(Capability::TokenWrite)
        .map_err(ApiError::from_missing_cap)?;

    let ticket = state
        .pairing
        .issue(
            state.tls_host.clone(),
            format!("sha256:{}", state.tls_fingerprint),
        )
        .await;

    let pairing_url = ticket.pairing_url();

    // Generate QR PNG — fall back gracefully if image encoding fails.
    let qr_png = taskagent_discovery::qr::encode_png(&pairing_url).unwrap_or_default();

    let response = serde_json::json!({
        "pairing_url": pairing_url,
        "expires_at": ticket.expires_at,
        "qr_png_base64": base64_encode(&qr_png),
        "host": ticket.host,
        "tls_fingerprint": format!("sha256:{}", ticket.tls_fingerprint),
    });
    Ok(Json(response))
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Constant-time string equality via XOR folding.
fn constant_eq_str(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}
