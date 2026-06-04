//! HTTP transport for flushing local replica events to a server.

use async_trait::async_trait;
use serde_json::json;
use taskagent_core::embed::EventEnvelope;
use taskagent_shared::{CoreError, Result};

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
        let base_url =
            std::env::var("TASKAGENT_API_URL").unwrap_or_else(|_| "http://localhost:8080".into());
        let token = std::env::var("TASKAGENT_TOKEN")
            .map_err(|_| CoreError::validation("TASKAGENT_TOKEN is required for sync"))?;
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
