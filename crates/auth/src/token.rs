//! [`ApiToken`] model + secret generation/hashing.

use std::fmt;

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHasher, SaltString},
    Argon2,
};
use base64::Engine as _;
use daruma_shared::{time, AgentId, Result, Timestamp, TokenId};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::scope::TokenScope;

/// Token kind — determines the human-readable prefix and the default scope
/// shape during creation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    /// Personal access token issued to a human user.
    Pat,
    /// Bot/agent token — scoped to a specific agent identity.
    Bot,
    /// Service token — internal/admin operations (typically Admin capability).
    Svc,
    /// User-issued token (e.g. remote device-code flow) — same actor semantics as [`Pat`].
    Usr,
    /// License token — signed offline entitlement carrier.
    License,
}

impl TokenKind {
    /// 3-letter component embedded in the rendered token string.
    pub fn slug(self) -> &'static str {
        match self {
            TokenKind::Pat => "pat",
            TokenKind::Bot => "bot",
            TokenKind::Svc => "svc",
            TokenKind::Usr => "usr",
            TokenKind::License => "lic",
        }
    }
}

/// Persistent token record (mirrors the `tokens` storage row).
///
/// `hash` holds the argon2id PHC string of the full rendered token. The
/// plaintext token is **only** returned at creation time inside
/// [`TokenSecret`] — never persisted, never logged.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApiToken {
    pub id: TokenId,
    /// First 12 characters of the rendered token, used to look up the row
    /// before the (expensive) argon2 verification.
    pub prefix: String,
    pub hash: String,
    pub kind: TokenKind,
    pub agent_id: AgentId,
    pub scope: TokenScope,
    pub rate_limit_per_min: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expired_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<Timestamp>,
}

impl ApiToken {
    /// True if the token is currently usable: not revoked, not expired.
    pub fn is_active(&self, now: Timestamp) -> bool {
        if self.revoked_at.is_some() {
            return false;
        }
        !matches!(self.expired_at, Some(exp) if exp <= now)
    }
}

/// Specification for creating a new token. The plaintext is computed by
/// [`generate`] and returned alongside the persistent record.
#[derive(Clone, Debug)]
pub struct NewTokenSpec {
    pub kind: TokenKind,
    pub agent_id: AgentId,
    pub scope: TokenScope,
    pub rate_limit_per_min: u32,
    pub expired_at: Option<Timestamp>,
}

/// Pair returned from [`generate`]: the persistent record + the plaintext
/// token string. The plaintext **must** be returned to the caller exactly
/// once and never stored.
#[derive(Clone)]
pub struct TokenSecret {
    pub record: ApiToken,
    pub plaintext: String,
}

impl fmt::Debug for TokenSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSecret")
            .field("record", &self.record)
            .field("plaintext", &"<redacted>")
            .finish()
    }
}

/// Generate a fresh token from `spec`. Hashes the plaintext with argon2id.
pub fn generate(spec: NewTokenSpec) -> Result<TokenSecret> {
    let plaintext = render_plaintext(spec.kind);
    let prefix = plaintext.chars().take(12).collect::<String>();
    let hash = hash_plaintext(&plaintext)?;

    let record = ApiToken {
        id: TokenId::new(),
        prefix,
        hash,
        kind: spec.kind,
        agent_id: spec.agent_id,
        scope: spec.scope,
        rate_limit_per_min: spec.rate_limit_per_min,
        tenant_id: None,
        created_at: time::now(),
        expired_at: spec.expired_at,
        last_used_at: None,
        revoked_at: None,
    };

    Ok(TokenSecret { record, plaintext })
}

/// Compute the 12-char prefix of a candidate token string.
pub fn prefix_of(token: &str) -> String {
    token.chars().take(12).collect()
}

/// argon2id verify of `candidate` against a stored PHC hash.
///
/// Returns `false` on any verification failure (mismatched, malformed
/// hash, etc.). Callers should treat `false` as "this token did not match
/// this row" and try the next candidate, not as a fatal error.
pub fn verify_plaintext(candidate: &str, hash: &str) -> bool {
    use argon2::PasswordVerifier;

    let Ok(parsed) = argon2::PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(candidate.as_bytes(), &parsed)
        .is_ok()
}

// ── private helpers ───────────────────────────────────────────────────────────

fn render_plaintext(kind: TokenKind) -> String {
    let mut bytes = [0u8; 24]; // 192 random bits → 32 b64url chars
    rand::thread_rng().fill_bytes(&mut bytes);
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    format!("ta_{}_{}", kind.slug(), secret)
}

fn hash_plaintext(plaintext: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plaintext.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| daruma_shared::CoreError::storage(format!("argon2 hash failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admin_spec() -> NewTokenSpec {
        NewTokenSpec {
            kind: TokenKind::Svc,
            agent_id: AgentId::new(),
            scope: TokenScope::admin(),
            rate_limit_per_min: 300,
            expired_at: None,
        }
    }

    #[test]
    fn generated_token_starts_with_ta_kind_prefix() {
        let secret = generate(admin_spec()).unwrap();
        assert!(secret.plaintext.starts_with("ta_svc_"));
        assert_eq!(secret.record.prefix.len(), 12);
        assert!(secret.plaintext.starts_with(&secret.record.prefix));
    }

    #[test]
    fn usr_kind_uses_ta_usr_prefix() {
        let spec = NewTokenSpec {
            kind: TokenKind::Usr,
            ..admin_spec()
        };
        let secret = generate(spec).unwrap();
        assert!(secret.plaintext.starts_with("ta_usr_"));
    }

    #[test]
    fn license_kind_uses_ta_lic_prefix() {
        let spec = NewTokenSpec {
            kind: TokenKind::License,
            ..admin_spec()
        };
        let secret = generate(spec).unwrap();
        assert!(secret.plaintext.starts_with("ta_lic_"));
    }

    #[test]
    fn verify_round_trip() {
        let secret = generate(admin_spec()).unwrap();
        assert!(verify_plaintext(&secret.plaintext, &secret.record.hash));
    }

    #[test]
    fn verify_rejects_wrong_token() {
        let secret = generate(admin_spec()).unwrap();
        assert!(!verify_plaintext(
            "ta_pat_wrong_token_string",
            &secret.record.hash
        ));
    }

    #[test]
    fn is_active_rejects_revoked_and_expired() {
        use chrono::Duration;
        let secret = generate(admin_spec()).unwrap();
        let now = time::now();
        assert!(secret.record.is_active(now));

        let mut revoked = secret.record.clone();
        revoked.revoked_at = Some(now);
        assert!(!revoked.is_active(now));

        let mut expired = secret.record.clone();
        expired.expired_at = Some(now - Duration::seconds(1));
        assert!(!expired.is_active(now));
    }

    #[test]
    fn token_secret_redacts_plaintext_in_debug() {
        let secret = generate(admin_spec()).unwrap();
        let dbg = format!("{:?}", secret);
        assert!(!dbg.contains(&secret.plaintext));
        assert!(dbg.contains("<redacted>"));
    }
}
