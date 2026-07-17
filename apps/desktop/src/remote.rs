//! HTTP transport for flushing local replica events to a server.

use async_trait::async_trait;
use daruma_core::embed::{EventEnvelope, Snapshot};
use daruma_shared::{CoreError, DeviceId, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::flush::RemoteEventSink;

pub struct HttpReplicaSink {
    client: reqwest::Client,
    base_url: String,
    token: String,
}

impl HttpReplicaSink {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
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

#[async_trait]
impl RemoteEventSink for HttpReplicaSink {
    async fn push(&self, envelope: EventEnvelope) -> Result<()> {
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
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(CoreError::sync(format!(
            "replica flush failed with {status}: {body}"
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
}
