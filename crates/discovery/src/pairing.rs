//! Single-use pairing token store with 5-minute TTL.
//!
//! ## Security invariants
//!
//! - Each token is cryptographically random (32 bytes / 256 bits).
//! - Tokens are single-use: [`PairingStore::consume`] atomically marks the
//!   token as used and will return `None` on any subsequent call with the same
//!   token (prevents replay attacks).
//! - All comparisons are done after HMAC-SHA256 keying, making them inherently
//!   constant-time at the verification step; the raw token is never directly
//!   compared with `==`.
//! - Tokens are never logged. Tracing lines use only a 6-char hint prefix.
//! - Expired tokens are swept lazily on every `consume` call, and
//!   proactively by [`PairingStore::sweep`].

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// TTL for pairing tickets.
const PAIRING_TTL_SECS: i64 = 5 * 60;

/// A single-use pairing ticket issued by the server.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairingTicket {
    /// Opaque token string — URL-safe base64 of 32 random bytes.
    pub token: String,
    /// When this ticket expires (UTC).
    pub expires_at: DateTime<Utc>,
    /// The host:port the client should connect to.
    pub host: String,
    /// TLS fingerprint in `sha256:<hex>` form.
    pub tls_fingerprint: String,
}

impl PairingTicket {
    /// The `daruma://pair?…` deep-link URL suitable for a QR code.
    pub fn pairing_url(&self) -> String {
        format!(
            "daruma://pair?host={}&token={}&fpr={}",
            urlencoding_encode(&self.host),
            urlencoding_encode(&self.token),
            urlencoding_encode(&self.tls_fingerprint),
        )
    }

    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at
    }
}

// Minimal URL percent-encoding — only encodes chars not safe in a query value.
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b':'
            | b'/'
            | b'@'
            | b'+'
            | b'=' => out.push(b as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ── Store ─────────────────────────────────────────────────────────────────────

struct Entry {
    ticket: PairingTicket,
    used: bool,
}

/// In-memory store of pending pairing tickets.
///
/// Cheap to clone — backed by an [`Arc<Mutex<_>>`].
#[derive(Clone)]
pub struct PairingStore {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
}

impl Default for PairingStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Issue a new single-use pairing ticket.
    pub async fn issue(&self, host: String, tls_fingerprint: String) -> PairingTicket {
        let token = generate_token();
        let ticket = PairingTicket {
            token: token.clone(),
            expires_at: Utc::now() + chrono::Duration::seconds(PAIRING_TTL_SECS),
            host,
            tls_fingerprint,
        };
        let hint = &token[..token.len().min(6)];
        tracing::debug!(token_hint = hint, "issued pairing ticket");

        let mut store = self.inner.lock().await;
        // Sweep expired entries while we hold the lock.
        store.retain(|_, e| !e.ticket.is_expired());
        store.insert(
            token,
            Entry {
                ticket: ticket.clone(),
                used: false,
            },
        );
        ticket
    }

    /// Attempt to consume a pairing ticket by its token string.
    ///
    /// Returns `Some(ticket)` exactly once for a valid, unexpired, unused
    /// ticket.  Returns `None` for unknown, expired, or already-used tokens.
    /// The comparison is performed via HMAC to avoid timing side-channels.
    pub async fn consume(&self, candidate: &str) -> Option<PairingTicket> {
        let mut store = self.inner.lock().await;
        // Sweep first to avoid returning stale data.
        store.retain(|_, e| !e.ticket.is_expired());

        // Find by constant-time HMAC comparison against all stored tokens.
        // We key the HMAC with a per-request nonce so that even if an
        // attacker observes timing they cannot map it to a specific stored token.
        let nonce: [u8; 16] = rand_bytes();
        let candidate_mac = hmac_tag(&nonce, candidate.as_bytes());

        let matched_key = store
            .iter()
            .filter(|(_, e)| !e.used)
            .find(|(k, _)| {
                let stored_mac = hmac_tag(&nonce, k.as_bytes());
                constant_eq(&candidate_mac, &stored_mac)
            })
            .map(|(k, _)| k.clone());

        if let Some(key) = matched_key {
            let entry = store.get_mut(&key).unwrap();
            entry.used = true;
            let ticket = entry.ticket.clone();
            let hint = &key[..key.len().min(6)];
            tracing::info!(token_hint = hint, "pairing ticket consumed");
            return Some(ticket);
        }

        tracing::warn!("pairing attempt with unknown/expired/used token");
        None
    }

    /// Remove expired entries (called periodically from the server).
    pub async fn sweep(&self) {
        let mut store = self.inner.lock().await;
        let before = store.len();
        store.retain(|_, e| !e.ticket.is_expired());
        let removed = before - store.len();
        if removed > 0 {
            tracing::debug!(removed, "swept expired pairing tickets");
        }
    }

    /// Number of active (non-expired, non-consumed) tickets.
    pub async fn active_count(&self) -> usize {
        let store = self.inner.lock().await;
        store
            .values()
            .filter(|e| !e.used && !e.ticket.is_expired())
            .count()
    }
}

// ── crypto helpers ─────────────────────────────────────────────────────────────

/// Generate a URL-safe base64 token from 32 random bytes.
fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn rand_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

/// HMAC-SHA256 tag (used for constant-time token lookup).
fn hmac_tag(key: &[u8], data: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Constant-time byte-slice equality.
fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn issue_and_consume() {
        let store = PairingStore::new();
        let ticket = store
            .issue("localhost:8443".into(), "sha256:abc".into())
            .await;
        assert!(!ticket.is_expired());
        let consumed = store.consume(&ticket.token).await;
        assert!(consumed.is_some());
        // Second consume must fail (single-use).
        let second = store.consume(&ticket.token).await;
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn wrong_token_returns_none() {
        let store = PairingStore::new();
        store
            .issue("localhost:8443".into(), "sha256:abc".into())
            .await;
        assert!(store.consume("not_a_real_token").await.is_none());
    }

    #[tokio::test]
    async fn pairing_url_format() {
        let store = PairingStore::new();
        let ticket = store
            .issue("192.168.1.5:8443".into(), "sha256:deadbeef".into())
            .await;
        let url = ticket.pairing_url();
        assert!(url.starts_with("daruma://pair?"));
        assert!(url.contains("host="));
        assert!(url.contains("token="));
        assert!(url.contains("fpr="));
    }
}
