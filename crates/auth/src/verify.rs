//! Stateless verifier — turn a bearer string + a [`TokenStore`] into either
//! an [`AuthContext`] or a structured failure that the HTTP/WS layer can
//! render into a 401.

use std::sync::Arc;

use daruma_shared::time;

use crate::context::AuthContext;
use crate::store::TokenStore;
use crate::token::{prefix_of, verify_plaintext};

/// Why bearer verification failed. Each variant maps cleanly to a 401 with
/// a stable error code on the HTTP side.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerifyError {
    /// Header missing or wrong scheme.
    Missing,
    /// Token format is obviously malformed (too short, wrong prefix).
    Malformed,
    /// Prefix not found in the store — no row to verify against.
    Unknown,
    /// Prefix matched but argon2 did not validate any candidate.
    Mismatch,
    /// Token row was found but is revoked.
    Revoked,
    /// Token row was found but has expired.
    Expired,
    /// Storage layer error during verification.
    Storage(String),
}

impl VerifyError {
    /// Stable error code (kebab-case) returned to clients.
    pub fn code(&self) -> &'static str {
        match self {
            VerifyError::Missing => "auth_missing",
            VerifyError::Malformed => "auth_malformed",
            VerifyError::Unknown => "auth_invalid",
            VerifyError::Mismatch => "auth_invalid",
            VerifyError::Revoked => "auth_revoked",
            VerifyError::Expired => "auth_expired",
            VerifyError::Storage(_) => "auth_storage",
        }
    }

    /// Human-readable detail. Never leaks the candidate token.
    pub fn message(&self) -> &str {
        match self {
            VerifyError::Missing => "missing Authorization bearer token",
            VerifyError::Malformed => "bearer token is malformed",
            VerifyError::Unknown => "bearer token is not recognised",
            VerifyError::Mismatch => "bearer token did not verify",
            VerifyError::Revoked => "bearer token has been revoked",
            VerifyError::Expired => "bearer token has expired",
            VerifyError::Storage(s) => s,
        }
    }
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for VerifyError {}

/// Verify a bearer-token string against a [`TokenStore`]. On success,
/// returns the [`AuthContext`] that should be inserted into the request.
///
/// Side-effects: best-effort `touch_last_used` on the matched row. Errors
/// from that call are swallowed — they must not fail the request.
pub async fn verify_bearer(
    store: &Arc<dyn TokenStore>,
    raw: &str,
) -> Result<AuthContext, VerifyError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(VerifyError::Missing);
    }
    // Accept either the full `Bearer <tok>` header value or the bare token.
    let bare = trimmed
        .strip_prefix("Bearer ")
        .or_else(|| trimmed.strip_prefix("bearer "))
        .unwrap_or(trimmed);

    if bare.len() < 14 || !bare.starts_with("ta_") {
        return Err(VerifyError::Malformed);
    }

    let prefix = prefix_of(bare);
    let candidates = store
        .list_by_prefix(&prefix)
        .await
        .map_err(|e| VerifyError::Storage(e.to_string()))?;

    if candidates.is_empty() {
        return Err(VerifyError::Unknown);
    }

    let now = time::now();
    let mut last_state_err = VerifyError::Mismatch;

    for candidate in candidates {
        if !verify_plaintext(bare, &candidate.hash) {
            continue;
        }
        // Hash matched — now classify lifecycle.
        if candidate.revoked_at.is_some() {
            last_state_err = VerifyError::Revoked;
            continue;
        }
        if let Some(exp) = candidate.expired_at {
            if exp <= now {
                last_state_err = VerifyError::Expired;
                continue;
            }
        }

        // Best-effort touch — ignore errors.
        let _ = store.touch_last_used(candidate.id).await;

        return Ok(AuthContext {
            agent_id: candidate.agent_id,
            token_id: candidate.id,
            tenant_id: candidate.tenant_id,
            rate_limit_per_min: candidate.rate_limit_per_min,
            scope: candidate.scope,
            token_kind: candidate.kind,
        });
    }

    Err(last_state_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::TokenScope;
    use crate::token::{generate, NewTokenSpec, TokenKind};
    use async_trait::async_trait;
    use std::sync::Mutex;
    use daruma_shared::{AgentId, Result, TokenId};

    // ── in-memory store for verifier tests ────────────────────────────────────

    #[derive(Default)]
    struct InMemoryStore {
        rows: Mutex<Vec<crate::token::ApiToken>>,
    }

    impl InMemoryStore {
        fn arc() -> Arc<dyn TokenStore> {
            Arc::new(Self::default())
        }
    }

    #[async_trait]
    impl TokenStore for InMemoryStore {
        async fn insert(&self, token: crate::token::ApiToken) -> Result<()> {
            self.rows.lock().unwrap().push(token);
            Ok(())
        }

        async fn list_by_prefix(&self, prefix: &str) -> Result<Vec<crate::token::ApiToken>> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.prefix == prefix)
                .cloned()
                .collect())
        }

        async fn get(&self, id: TokenId) -> Result<Option<crate::token::ApiToken>> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .find(|t| t.id == id)
                .cloned())
        }

        async fn list_for_agent(&self, agent_id: AgentId) -> Result<Vec<crate::token::ApiToken>> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.agent_id == agent_id)
                .cloned()
                .collect())
        }

        async fn revoke(&self, id: TokenId) -> Result<bool> {
            let mut rows = self.rows.lock().unwrap();
            if let Some(row) = rows.iter_mut().find(|t| t.id == id) {
                row.revoked_at = Some(time::now());
                Ok(true)
            } else {
                Ok(false)
            }
        }

        async fn touch_last_used(&self, _id: TokenId) -> Result<()> {
            Ok(())
        }

        async fn count_active(&self) -> Result<u64> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.revoked_at.is_none())
                .count() as u64)
        }
    }

    fn admin_spec() -> NewTokenSpec {
        NewTokenSpec {
            kind: TokenKind::Svc,
            agent_id: AgentId::new(),
            scope: TokenScope::admin(),
            rate_limit_per_min: 300,
            expired_at: None,
        }
    }

    #[tokio::test]
    async fn verify_returns_context_for_valid_token() {
        let store = InMemoryStore::arc();
        let secret = generate(admin_spec()).unwrap();
        store.insert(secret.record.clone()).await.unwrap();

        let ctx = verify_bearer(&store, &secret.plaintext).await.unwrap();
        assert_eq!(ctx.token_id, secret.record.id);
        assert_eq!(ctx.agent_id, secret.record.agent_id);
    }

    #[tokio::test]
    async fn verify_rejects_missing() {
        let store = InMemoryStore::arc();
        let err = verify_bearer(&store, "").await.unwrap_err();
        assert_eq!(err, VerifyError::Missing);
    }

    #[tokio::test]
    async fn verify_rejects_malformed() {
        let store = InMemoryStore::arc();
        let err = verify_bearer(&store, "Bearer xx").await.unwrap_err();
        assert_eq!(err, VerifyError::Malformed);
    }

    #[tokio::test]
    async fn verify_rejects_unknown_prefix() {
        let store = InMemoryStore::arc();
        let err = verify_bearer(&store, "ta_pat_unknown_prefix_for_testing")
            .await
            .unwrap_err();
        assert_eq!(err, VerifyError::Unknown);
    }

    #[tokio::test]
    async fn verify_rejects_revoked() {
        let store = InMemoryStore::arc();
        let secret = generate(admin_spec()).unwrap();
        store.insert(secret.record.clone()).await.unwrap();
        store.revoke(secret.record.id).await.unwrap();

        let err = verify_bearer(&store, &secret.plaintext).await.unwrap_err();
        assert_eq!(err, VerifyError::Revoked);
    }

    #[tokio::test]
    async fn accepts_bearer_prefix_or_bare() {
        let store = InMemoryStore::arc();
        let secret = generate(admin_spec()).unwrap();
        store.insert(secret.record.clone()).await.unwrap();

        let with_prefix = format!("Bearer {}", secret.plaintext);
        assert!(verify_bearer(&store, &with_prefix).await.is_ok());
        assert!(verify_bearer(&store, &secret.plaintext).await.is_ok());
    }
}
