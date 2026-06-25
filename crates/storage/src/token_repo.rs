//! SQLite-backed implementation of [`daruma_auth::TokenStore`].
//!
//! Tokens are stored in the `tokens` table (migration `0004_tokens.sql`).
//! `scope` is encoded as JSON in `scope_json`; `kind` is stored as the
//! kebab-case `TokenKind::slug()` so SQL filtering by kind is readable.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use daruma_auth::{ApiToken, TokenKind, TokenScope, TokenStore};
use daruma_shared::{time, AgentId, CoreError, Result, TokenId};

/// Read/write access to the `tokens` table.
#[derive(Clone)]
pub struct TokenRepo {
    pool: SqlitePool,
}

impl TokenRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TokenStore for TokenRepo {
    async fn insert(&self, token: ApiToken) -> Result<()> {
        let scope_json =
            serde_json::to_string(&token.scope).map_err(|e| CoreError::serde(e.to_string()))?;

        sqlx::query(
            "INSERT INTO tokens \
             (id, prefix, hash, kind, agent_id, scope_json, rate_limit_per_min, \
              created_at, expired_at, last_used_at, revoked_at, tenant_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(token.id.to_string())
        .bind(&token.prefix)
        .bind(&token.hash)
        .bind(kind_slug(token.kind))
        .bind(token.agent_id.to_string())
        .bind(scope_json)
        .bind(token.rate_limit_per_min as i64)
        .bind(token.created_at.to_rfc3339())
        .bind(token.expired_at.map(|t| t.to_rfc3339()))
        .bind(token.last_used_at.map(|t| t.to_rfc3339()))
        .bind(token.revoked_at.map(|t| t.to_rfc3339()))
        .bind(token.tenant_id)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(())
    }

    async fn list_by_prefix(&self, prefix: &str) -> Result<Vec<ApiToken>> {
        let rows = sqlx::query(SELECT_COLS_FROM_TOKENS)
            .bind(prefix)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.iter().map(row_to_token).collect()
    }

    async fn get(&self, id: TokenId) -> Result<Option<ApiToken>> {
        let row = sqlx::query(
            "SELECT id, prefix, hash, kind, agent_id, scope_json, rate_limit_per_min, \
             tenant_id, created_at, expired_at, last_used_at, revoked_at \
             FROM tokens WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(row_to_token).transpose()
    }

    async fn list_for_agent(&self, agent_id: AgentId) -> Result<Vec<ApiToken>> {
        let rows = sqlx::query(
            "SELECT id, prefix, hash, kind, agent_id, scope_json, rate_limit_per_min, \
             tenant_id, created_at, expired_at, last_used_at, revoked_at \
             FROM tokens WHERE agent_id = ? ORDER BY created_at DESC",
        )
        .bind(agent_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_token).collect()
    }

    async fn revoke(&self, id: TokenId) -> Result<bool> {
        let res =
            sqlx::query("UPDATE tokens SET revoked_at = COALESCE(revoked_at, ?) WHERE id = ?")
                .bind(time::now().to_rfc3339())
                .bind(id.to_string())
                .execute(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(res.rows_affected() > 0)
    }

    async fn touch_last_used(&self, id: TokenId) -> Result<()> {
        sqlx::query("UPDATE tokens SET last_used_at = ? WHERE id = ?")
            .bind(time::now().to_rfc3339())
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    async fn count_active(&self) -> Result<u64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM tokens WHERE revoked_at IS NULL")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let n: i64 = row
            .try_get("n")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(n as u64)
    }
}

// ── row mapping ───────────────────────────────────────────────────────────────

const SELECT_COLS_FROM_TOKENS: &str = "SELECT id, prefix, hash, kind, agent_id, scope_json, \
     rate_limit_per_min, tenant_id, created_at, expired_at, last_used_at, revoked_at \
     FROM tokens WHERE prefix = ?";

fn row_to_token(row: &sqlx::sqlite::SqliteRow) -> Result<ApiToken> {
    let id_s: String = row
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let prefix: String = row
        .try_get("prefix")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let hash: String = row
        .try_get("hash")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let kind_s: String = row
        .try_get("kind")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let agent_id_s: String = row
        .try_get("agent_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let scope_json: String = row
        .try_get("scope_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let rate_limit_per_min: i64 = row
        .try_get("rate_limit_per_min")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let tenant_id: Option<String> = row
        .try_get("tenant_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let created_at_s: String = row
        .try_get("created_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let expired_at_s: Option<String> = row
        .try_get("expired_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let last_used_at_s: Option<String> = row
        .try_get("last_used_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let revoked_at_s: Option<String> = row
        .try_get("revoked_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    let id = id_s
        .parse::<TokenId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let agent_id = agent_id_s
        .parse::<AgentId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let scope: TokenScope =
        serde_json::from_str(&scope_json).map_err(|e| CoreError::serde(e.to_string()))?;

    Ok(ApiToken {
        id,
        prefix,
        hash,
        kind: parse_kind(&kind_s)?,
        agent_id,
        scope,
        rate_limit_per_min: rate_limit_per_min.max(0) as u32,
        tenant_id,
        created_at: parse_ts(&created_at_s)?,
        expired_at: expired_at_s.map(|s| parse_ts(&s)).transpose()?,
        last_used_at: last_used_at_s.map(|s| parse_ts(&s)).transpose()?,
        revoked_at: revoked_at_s.map(|s| parse_ts(&s)).transpose()?,
    })
}

fn parse_kind(s: &str) -> Result<TokenKind> {
    match s {
        "pat" => Ok(TokenKind::Pat),
        "bot" => Ok(TokenKind::Bot),
        "svc" => Ok(TokenKind::Svc),
        "usr" => Ok(TokenKind::Usr),
        "lic" => Ok(TokenKind::License),
        other => Err(CoreError::serde(format!("unknown token kind: {other}"))),
    }
}

fn kind_slug(k: TokenKind) -> &'static str {
    k.slug()
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| CoreError::serde(e.to_string()))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_auth::{generate, NewTokenSpec, TokenScope};

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
    async fn insert_and_list_by_prefix() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = TokenRepo::new(db.pool().clone());

        let secret = generate(admin_spec()).unwrap();
        repo.insert(secret.record.clone()).await.unwrap();

        let by_prefix = repo.list_by_prefix(&secret.record.prefix).await.unwrap();
        assert_eq!(by_prefix.len(), 1);
        assert_eq!(by_prefix[0].id, secret.record.id);
    }

    #[tokio::test]
    async fn revoke_idempotent_and_active_count() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = TokenRepo::new(db.pool().clone());

        let secret = generate(admin_spec()).unwrap();
        repo.insert(secret.record.clone()).await.unwrap();
        assert_eq!(repo.count_active().await.unwrap(), 1);

        assert!(repo.revoke(secret.record.id).await.unwrap());
        assert!(repo.revoke(secret.record.id).await.unwrap()); // idempotent
        assert_eq!(repo.count_active().await.unwrap(), 0);
    }
}
