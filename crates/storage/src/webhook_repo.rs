//! SQLite-backed implementation of [`daruma_webhooks::WebhookStore`].

use crate::parse_ts;
use async_trait::async_trait;
use daruma_auth::ProjectFilter;
use daruma_shared::{time, CoreError, EventId, Result, WebhookDeliveryId, WebhookId};
use daruma_webhooks::{Webhook, WebhookPatch, WebhookStore};
use sqlx::{Row, SqlitePool};

/// Read/write access to the `webhooks` + `webhook_deliveries` tables.
#[derive(Clone)]
pub struct WebhookRepo {
    pool: SqlitePool,
}

impl WebhookRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl WebhookStore for WebhookRepo {
    async fn insert(&self, w: Webhook) -> Result<()> {
        let events_json =
            serde_json::to_string(&w.events).map_err(|e| CoreError::serde(e.to_string()))?;
        let filter_json = serde_json::to_string(&w.project_filter)
            .map_err(|e| CoreError::serde(e.to_string()))?;
        let enrich_json =
            serde_json::to_string(&w.enrich).map_err(|e| CoreError::serde(e.to_string()))?;

        sqlx::query(
            "INSERT INTO webhooks (id, url, secret, events_json, project_filter_json, \
             is_active, description, enrich_json, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(w.id.to_string())
        .bind(&w.url)
        .bind(&w.secret)
        .bind(events_json)
        .bind(filter_json)
        .bind(if w.is_active { 1_i32 } else { 0 })
        .bind(&w.description)
        .bind(enrich_json)
        .bind(w.created_at.to_rfc3339())
        .bind(w.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    async fn get(&self, id: WebhookId) -> Result<Option<Webhook>> {
        let row = sqlx::query(SELECT_WEBHOOK_BY_ID)
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        row.as_ref().map(row_to_webhook).transpose()
    }

    async fn list_active(&self) -> Result<Vec<Webhook>> {
        let rows = sqlx::query(SELECT_WEBHOOKS_ACTIVE)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.iter().map(row_to_webhook).collect()
    }

    async fn list_all(&self) -> Result<Vec<Webhook>> {
        let rows = sqlx::query(SELECT_WEBHOOKS_ALL)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.iter().map(row_to_webhook).collect()
    }

    async fn patch(&self, id: WebhookId, patch: WebhookPatch) -> Result<Option<Webhook>> {
        let Some(mut w) = self.get(id).await? else {
            return Ok(None);
        };
        patch.apply(&mut w);

        let events_json =
            serde_json::to_string(&w.events).map_err(|e| CoreError::serde(e.to_string()))?;
        let filter_json = serde_json::to_string(&w.project_filter)
            .map_err(|e| CoreError::serde(e.to_string()))?;
        let enrich_json =
            serde_json::to_string(&w.enrich).map_err(|e| CoreError::serde(e.to_string()))?;

        sqlx::query(
            "UPDATE webhooks SET url = ?, secret = ?, events_json = ?, \
             project_filter_json = ?, is_active = ?, description = ?, \
             enrich_json = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(&w.url)
        .bind(&w.secret)
        .bind(events_json)
        .bind(filter_json)
        .bind(if w.is_active { 1_i32 } else { 0 })
        .bind(&w.description)
        .bind(enrich_json)
        .bind(w.updated_at.to_rfc3339())
        .bind(w.id.to_string())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(Some(w))
    }

    async fn delete(&self, id: WebhookId) -> Result<bool> {
        let res = sqlx::query("DELETE FROM webhooks WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(res.rows_affected() > 0)
    }

    #[allow(clippy::too_many_arguments)]
    async fn record_delivery(
        &self,
        webhook_id: WebhookId,
        event_id: EventId,
        event_kind: &str,
        status_code: Option<u16>,
        succeeded: bool,
        attempts: u32,
        error: Option<&str>,
    ) -> Result<()> {
        let delivery_id = WebhookDeliveryId::new();
        sqlx::query(
            "INSERT INTO webhook_deliveries (id, webhook_id, event_id, event_kind, \
             status_code, succeeded, attempts, error, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(delivery_id.to_string())
        .bind(webhook_id.to_string())
        .bind(event_id.to_string())
        .bind(event_kind)
        .bind(status_code.map(|c| c as i64))
        .bind(if succeeded { 1_i32 } else { 0 })
        .bind(attempts as i64)
        .bind(error)
        .bind(time::now().to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }
}

// Read-only helpers exposed for admin-side listing of deliveries (not on
// the trait yet — keep the surface small).
impl WebhookRepo {
    pub async fn count_deliveries_for(&self, webhook_id: WebhookId) -> Result<u64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM webhook_deliveries WHERE webhook_id = ?")
            .bind(webhook_id.to_string())
            .fetch_one(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let n: i64 = row
            .try_get("n")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(n.max(0) as u64)
    }
}

// ── row mapping ───────────────────────────────────────────────────────────────

const SELECT_WEBHOOK_BY_ID: &str =
    "SELECT id, url, secret, events_json, project_filter_json, is_active, description, \
     enrich_json, created_at, updated_at FROM webhooks WHERE id = ?";

const SELECT_WEBHOOKS_ACTIVE: &str =
    "SELECT id, url, secret, events_json, project_filter_json, is_active, description, \
     enrich_json, created_at, updated_at \
     FROM webhooks WHERE is_active = 1 ORDER BY created_at ASC";

const SELECT_WEBHOOKS_ALL: &str =
    "SELECT id, url, secret, events_json, project_filter_json, is_active, description, \
     enrich_json, created_at, updated_at FROM webhooks ORDER BY created_at ASC";

fn row_to_webhook(row: &sqlx::sqlite::SqliteRow) -> Result<Webhook> {
    let id_s: String = row
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let url: String = row
        .try_get("url")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let secret: String = row
        .try_get("secret")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let events_json: String = row
        .try_get("events_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let filter_json: String = row
        .try_get("project_filter_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let is_active: i64 = row
        .try_get("is_active")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let description: Option<String> = row
        .try_get("description")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    // `enrich_json` was added in migration 0012; rows that predate the
    // migration receive the column default ('[]') so deserialising never
    // panics on an upgrade.
    let enrich_json: String = row
        .try_get("enrich_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let created_s: String = row
        .try_get("created_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let updated_s: String = row
        .try_get("updated_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    let id: WebhookId = id_s
        .parse()
        .map_err(|e: uuid::Error| CoreError::serde(e.to_string()))?;
    let events: Vec<String> =
        serde_json::from_str(&events_json).map_err(|e| CoreError::serde(e.to_string()))?;
    let project_filter: ProjectFilter =
        serde_json::from_str(&filter_json).map_err(|e| CoreError::serde(e.to_string()))?;
    let enrich: Vec<String> =
        serde_json::from_str(&enrich_json).map_err(|e| CoreError::serde(e.to_string()))?;

    Ok(Webhook {
        id,
        url,
        secret,
        events,
        project_filter,
        is_active: is_active != 0,
        description,
        enrich,
        created_at: parse_ts(&created_s)?,
        updated_at: parse_ts(&updated_s)?,
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_webhooks::NewWebhook;

    async fn make_repo() -> WebhookRepo {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        WebhookRepo::new(db.pool().clone())
    }

    #[tokio::test]
    async fn insert_list_active_excludes_inactive() {
        let repo = make_repo().await;
        let a = NewWebhook {
            id: None,
            url: "https://a".into(),
            secret: "s".into(),
            events: vec![],
            project_filter: ProjectFilter::All,
            is_active: true,
            description: None,
            enrich: vec![],
        }
        .into_webhook();
        let b = NewWebhook {
            id: None,
            url: "https://b".into(),
            secret: "s".into(),
            events: vec![],
            project_filter: ProjectFilter::All,
            is_active: false,
            description: None,
            enrich: vec![],
        }
        .into_webhook();
        repo.insert(a.clone()).await.unwrap();
        repo.insert(b.clone()).await.unwrap();

        let active = repo.list_active().await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, a.id);

        let all = repo.list_all().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn patch_updates_only_supplied_fields() {
        let repo = make_repo().await;
        let original = NewWebhook {
            id: None,
            url: "https://a".into(),
            secret: "s".into(),
            events: vec!["task_created".into()],
            project_filter: ProjectFilter::All,
            is_active: true,
            description: Some("orig".into()),
            enrich: vec![],
        }
        .into_webhook();
        repo.insert(original.clone()).await.unwrap();

        let patched = repo
            .patch(
                original.id,
                WebhookPatch {
                    url: Some("https://b".into()),
                    ..WebhookPatch::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(patched.url, "https://b");
        assert_eq!(patched.secret, "s"); // unchanged
        assert_eq!(patched.events, vec!["task_created".to_string()]);
    }
}
