//! HTTP transport for flushing local replica events to a server.

use async_trait::async_trait;
use daruma_core::embed::{EventEnvelope, Snapshot};
use daruma_shared::{CoreError, DeviceId, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::flush::{PushOutcome, RemoteEventSink};

pub struct HttpReplicaSink {
    client: reqwest::Client,
    base_url: String,
    token: String,
}

impl HttpReplicaSink {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        // Не удалять: тестовые процессы не запускают main; reqwest с
        // `rustls-no-provider` требует CryptoProvider::get_default().
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();

        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            token: token.into(),
        }
    }

    pub fn from_env() -> Result<Self> {
        let paired = load_paired_credentials();
        let base_url = std::env::var("DARUMA_API_URL")
            .ok()
            .or_else(|| paired.as_ref().map(|p| p.server_url.clone()))
            .unwrap_or_else(|| "http://localhost:8080".into());
        let token = std::env::var("DARUMA_TOKEN")
            .ok()
            .or_else(|| paired.map(|p| p.token))
            .ok_or_else(|| CoreError::validation("DARUMA_TOKEN is required for sync"))?;
        Ok(Self::new(base_url, token))
    }

    fn replica_url(&self) -> String {
        format!("{}/v1/events/replica", self.base_url.trim_end_matches('/'))
    }

    fn events_url(&self, since: u64, limit: u32) -> String {
        format!(
            "{}/v1/events?since={since}&limit={limit}",
            self.base_url.trim_end_matches('/')
        )
    }

    fn snapshot_url(&self) -> String {
        format!("{}/v1/events/snapshot", self.base_url.trim_end_matches('/'))
    }

    fn devices_url(&self) -> String {
        format!("{}/v1/devices", self.base_url.trim_end_matches('/'))
    }

    fn revoke_device_url(&self, id: DeviceId) -> String {
        format!(
            "{}/v1/devices/{id}/revoke",
            self.base_url.trim_end_matches('/')
        )
    }

    pub async fn fetch_events(&self, since: u64, limit: u32) -> Result<Vec<EventEnvelope>> {
        let response = self
            .client
            .get(self.events_url(since, limit))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| CoreError::sync(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CoreError::sync(format!(
                "replica fetch failed with {status}: {body}"
            )));
        }
        response
            .json::<Vec<EventEnvelope>>()
            .await
            .map_err(|e| CoreError::serde(e.to_string()))
    }

    /// Fetch the latest bootstrap snapshot for catch-up, if the server has
    /// one. `Ok(None)` covers both "writer has not produced a snapshot yet"
    /// (200 with a `null` body) and older servers without the endpoint
    /// (404) — in both cases the caller falls back to a full replay.
    pub async fn fetch_snapshot(&self) -> Result<Option<Snapshot>> {
        let response = self
            .client
            .get(self.snapshot_url())
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| CoreError::sync(e.to_string()))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        json_response(response, "snapshot fetch").await
    }

    pub async fn list_devices(&self) -> Result<DevicesResponse> {
        let response = self
            .client
            .get(self.devices_url())
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| CoreError::sync(e.to_string()))?;
        json_response(response, "device list").await
    }

    pub async fn revoke_device(&self, id: DeviceId) -> Result<()> {
        let response = self
            .client
            .post(self.revoke_device_url(id))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| CoreError::sync(e.to_string()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(CoreError::sync(format!(
            "device revoke failed with {status}: {body}"
        )))
    }
}

/// Server error codes that refuse one specific event for good: such an event
/// is dead-lettered and the flush moves on.
const EVENT_REJECTION_CODES: &[&str] = &["intake_source_admin_only"];

#[async_trait]
impl RemoteEventSink for HttpReplicaSink {
    async fn push(&self, envelope: EventEnvelope) -> Result<PushOutcome> {
        let response = self
            .client
            .post(self.replica_url())
            .bearer_auth(&self.token)
            .json(&json!({ "events": [envelope] }))
            .send()
            .await
            .map_err(|e| CoreError::sync(e.to_string()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(PushOutcome::Accepted);
        }
        let body = response.text().await.unwrap_or_default();
        // Error body: OSS `{"error":{"code","message"}}` or cloud
        // `{"error":"<code>","message"}`.
        let parsed = serde_json::from_str::<serde_json::Value>(&body).unwrap_or_default();
        let code = parsed["error"]["code"]
            .as_str()
            .or_else(|| parsed["error"].as_str())
            .unwrap_or_default();
        // Only a code that judges this one event dead-letters it; anything
        // else (wrong URL, auth, quota, 5xx) stops the flush for a retry.
        if status.is_client_error() && EVENT_REJECTION_CODES.contains(&code) {
            return Ok(PushOutcome::Rejected {
                status: status.as_u16(),
                code: code.to_owned(),
            });
        }
        let message = parsed["error"]["message"]
            .as_str()
            .or_else(|| parsed["message"].as_str())
            .unwrap_or(&body);
        Err(CoreError::sync(format!(
            "replica flush failed with {status} (code `{code}`): {message}"
        )))
    }
}

async fn json_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    action: &str,
) -> Result<T> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(CoreError::sync(format!(
            "{action} failed with {status}: {body}"
        )));
    }
    response
        .json::<T>()
        .await
        .map_err(|e| CoreError::serde(e.to_string()))
}

#[derive(Debug, Deserialize)]
struct PairedCredentials {
    server_url: String,
    token: String,
}

fn load_paired_credentials() -> Option<PairedCredentials> {
    let path = crate::onboarding::paired_credentials_path();
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Device {
    pub id: DeviceId,
    pub label: String,
    pub created_at: String,
    pub last_seen_at: Option<String>,
    pub revoked_at: Option<String>,
    pub connected: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DevicesResponse {
    pub current_device_id: Option<DeviceId>,
    pub devices: Vec<Device>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replica_url_trims_trailing_slash() {
        let sink = HttpReplicaSink::new("http://localhost:8080/", "token");
        assert_eq!(
            sink.replica_url(),
            "http://localhost:8080/v1/events/replica"
        );
    }

    #[test]
    fn events_url_includes_since_and_limit() {
        let sink = HttpReplicaSink::new("http://localhost:8080/", "token");
        assert_eq!(
            sink.events_url(12, 50),
            "http://localhost:8080/v1/events?since=12&limit=50"
        );
    }

    /// A one-shot HTTP server answering each request with the next scripted
    /// `(status, body)`; returns its base URL and the request counter.
    async fn scripted_server(
        replies: Vec<(u16, &'static str)>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read the head, then the Content-Length body.
                let (head_end, len) = loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "client closed before the request head");
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map_or(0, |v| v.trim().parse().unwrap());
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let n = socket.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
                socket.shutdown().await.ok();
            }
        });
        (format!("http://{addr}"), hits)
    }

    async fn outbox_with(events: u64) -> crate::outbox::Outbox {
        use daruma_core::embed::{Db, Event};
        let outbox = crate::outbox::Outbox::new(Db::memory().await.unwrap());
        outbox.ensure_schema().await.unwrap();
        let device = DeviceId::new();
        for seq in 1..=events {
            let envelope = EventEnvelope::new(
                daruma_domain::Actor::user(),
                Event::TaskCreated {
                    task: daruma_domain::NewTask::new(format!("t{seq}")),
                },
            );
            outbox.enqueue(device, seq, envelope).await.unwrap();
        }
        outbox
    }

    const REJECTED: &str =
        r#"{"error":"intake_source_admin_only","message":"only people change intake_source"}"#;

    #[tokio::test]
    async fn final_4xx_is_dead_lettered_and_the_rest_still_flush() {
        let (url, hits) = scripted_server(vec![(200, "{}"), (403, REJECTED), (200, "{}")]).await;
        let outbox = outbox_with(3).await;
        let sink = HttpReplicaSink::new(url, "token");

        let stats = crate::flush::flush_pending(&outbox, &sink, 100)
            .await
            .unwrap();
        assert_eq!((stats.attempted, stats.flushed, stats.rejected), (3, 2, 1));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(outbox.pending(100).await.unwrap().is_empty());
        assert_eq!(outbox.rejected_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn server_error_stops_the_flush_without_dead_letter() {
        let (url, _) = scripted_server(vec![(500, "{}")]).await;
        let outbox = outbox_with(2).await;
        let sink = HttpReplicaSink::new(url, "token");

        assert!(crate::flush::flush_pending(&outbox, &sink, 100)
            .await
            .is_err());
        assert_eq!(outbox.pending(100).await.unwrap().len(), 2);
        assert_eq!(outbox.rejected_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn too_many_requests_is_retried_on_the_next_flush() {
        let (url, _) = scripted_server(vec![(429, "{}"), (200, "{}")]).await;
        let outbox = outbox_with(1).await;
        let sink = HttpReplicaSink::new(url, "token");

        assert!(crate::flush::flush_pending(&outbox, &sink, 100)
            .await
            .is_err());
        assert_eq!(outbox.pending(100).await.unwrap().len(), 1);
        let stats = crate::flush::flush_pending(&outbox, &sink, 100)
            .await
            .unwrap();
        assert_eq!((stats.flushed, stats.rejected), (1, 0));
        assert_eq!(outbox.rejected_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn listed_code_in_oss_error_shape_is_dead_lettered_too() {
        let (url, _) = scripted_server(vec![(
            403,
            r#"{"error":{"code":"intake_source_admin_only","message":"people only"}}"#,
        )])
        .await;
        let outbox = outbox_with(1).await;
        let sink = HttpReplicaSink::new(url, "token");

        let stats = crate::flush::flush_pending(&outbox, &sink, 100)
            .await
            .unwrap();
        assert_eq!(stats.rejected, 1);
        assert_eq!(outbox.rejected_count().await.unwrap(), 1);
    }

    /// A 4xx without a listed code stops the flush: the event stays queued.
    #[tokio::test]
    async fn unlisted_4xx_stops_the_flush_and_keeps_the_event() {
        for (status, body, needle) in [
            (
                403,
                r#"{"error":{"code":"forbidden","message":"no"}}"#,
                "code `forbidden`): no",
            ),
            (
                400,
                r#"{"error":"validation","message":"batch exceeds hard cap"}"#,
                "code `validation`): batch exceeds hard cap",
            ),
            (
                409,
                r#"{"error":{"code":"conflict","message":"dup"}}"#,
                "code `conflict`): dup",
            ),
            (404, "not found", "404"),
        ] {
            let (url, _) = scripted_server(vec![(status, body)]).await;
            let outbox = outbox_with(2).await;
            let sink = HttpReplicaSink::new(url, "token");

            let err = crate::flush::flush_pending(&outbox, &sink, 100)
                .await
                .unwrap_err();
            let err = err.to_string();
            assert!(
                err.contains(&status.to_string()) && err.contains(needle),
                "{err}"
            );
            assert_eq!(outbox.pending(100).await.unwrap().len(), 2, "{status}");
            assert_eq!(outbox.rejected_count().await.unwrap(), 0, "{status}");
        }
    }
}
