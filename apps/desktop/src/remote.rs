//! HTTP transport for flushing local replica events to a server.

use async_trait::async_trait;
use daruma_core::embed::{EventEnvelope, Snapshot};
use daruma_shared::{CoreError, DeviceId, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::flush::{PushOutcome, RemoteEventSink};
use crate::replica::ReplicaSource;

pub struct HttpReplicaSink {
    client: reqwest::Client,
    base_url: String,
    token: String,
    /// What to do about it when TLS refuses this server.
    tls_refusal_hint: Option<&'static str>,
}

/// What `sync` and `devices` accept as the server's certificate.
#[derive(Debug, PartialEq, Eq)]
enum ServerTrust {
    /// The paired server: only the certificate verified at pairing.
    Pinned { fingerprint: String },
    /// The paired server, paired before fingerprints were stored. The system
    /// decides; nothing was verified that could be pinned after the fact.
    PairedWithoutPin,
    /// Any other server: the system decides. A pin stays with its server.
    System,
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
            tls_refusal_hint: None,
        }
    }

    fn with_trust(
        base_url: impl Into<String>,
        token: impl Into<String>,
        trust: ServerTrust,
    ) -> Result<Self> {
        let mut sink = Self::new(base_url, token);
        match trust {
            ServerTrust::Pinned { fingerprint } => {
                sink.client = pinned_client(&fingerprint)?;
                sink.tls_refusal_hint = Some(
                    "The bearer token was not sent. If the server's certificate was replaced \
                     on purpose, confirm its new fingerprint with the server's owner and run \
                     `daruma pair` with a fresh ticket.",
                );
            }
            ServerTrust::PairedWithoutPin => {
                sink.tls_refusal_hint = Some(
                    "This pairing has no stored certificate fingerprint and the system does \
                     not trust the server. Get a fresh ticket from the server's owner and run \
                     `daruma pair` to pin its certificate.",
                );
            }
            ServerTrust::System => {}
        }
        Ok(sink)
    }

    pub fn from_env() -> Result<Self> {
        let paired = load_paired_credentials();
        let base_url = std::env::var("DARUMA_API_URL")
            .ok()
            .or_else(|| paired.as_ref().map(|p| p.server_url.clone()))
            .unwrap_or_else(|| "http://localhost:8080".into());
        let trust = server_trust(&base_url, paired.as_ref());
        let token = std::env::var("DARUMA_TOKEN")
            .ok()
            .or_else(|| paired.map(|p| p.token))
            .ok_or_else(|| CoreError::validation("DARUMA_TOKEN is required for sync"))?;
        Self::with_trust(base_url, token, trust)
    }

    /// A request that never completed; a TLS refusal says what it means.
    fn transport_error(&self, error: reqwest::Error) -> CoreError {
        match (tls_refusal(&error), self.tls_refusal_hint) {
            (Some(refusal), Some(hint)) => {
                CoreError::sync(format!("TLS refused {}: {refusal}. {hint}", self.base_url))
            }
            (Some(refusal), None) => {
                CoreError::sync(format!("TLS refused {}: {refusal}", self.base_url))
            }
            (None, _) => CoreError::sync(error.to_string()),
        }
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

    pub async fn list_devices(&self) -> Result<DevicesResponse> {
        let response = self
            .client
            .get(self.devices_url())
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        json_response(response, "device list").await
    }

    pub async fn revoke_device(&self, id: DeviceId) -> Result<()> {
        let response = self
            .client
            .post(self.revoke_device_url(id))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
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
impl ReplicaSource for HttpReplicaSink {
    async fn fetch_events(&self, since: u64, limit: u32) -> Result<Vec<EventEnvelope>> {
        let response = self
            .client
            .get(self.events_url(since, limit))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
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
    async fn fetch_snapshot(&self) -> Result<Option<Snapshot>> {
        let response = self
            .client
            .get(self.snapshot_url())
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        json_response(response, "snapshot fetch").await
    }
}

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
            .map_err(|e| self.transport_error(e))?;
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
    /// Absent in a `paired.json` written before fingerprints were stored.
    #[serde(default)]
    tls_fingerprint: Option<String>,
}

/// Trust for `base_url`: the pin applies only to the scheme, host and port it
/// was verified for, so `DARUMA_API_URL` pointing elsewhere does not carry it.
fn server_trust(base_url: &str, paired: Option<&PairedCredentials>) -> ServerTrust {
    let Some(paired) = paired.filter(|p| same_origin(base_url, &p.server_url)) else {
        return ServerTrust::System;
    };
    match &paired.tls_fingerprint {
        Some(fingerprint) => ServerTrust::Pinned {
            fingerprint: fingerprint.clone(),
        },
        None => ServerTrust::PairedWithoutPin,
    }
}

fn same_origin(left: &str, right: &str) -> bool {
    match (origin(left), origin(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn origin(url: &str) -> Option<(String, String, u16)> {
    let url = reqwest::Url::parse(url).ok()?;
    let host = url.host_str()?.to_owned();
    Some((url.scheme().to_owned(), host, url.port_or_known_default()?))
}

/// A client that talks only to the certificate behind `fingerprint`. A stored
/// fingerprint that cannot be read fails here rather than falling back to
/// another trust. Redirects stay unfollowed: the pin belongs to this server.
fn pinned_client(fingerprint: &str) -> Result<reqwest::Client> {
    let hex = fingerprint.strip_prefix("sha256:").unwrap_or(fingerprint);
    let tls = crate::onboarding::pinned_tls_config(hex)
        .map_err(|e| CoreError::validation(format!("paired.json tls_fingerprint: {e:#}")))?;
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| CoreError::sync(e.to_string()))
}

/// The rustls error behind a failed request, if TLS is why it failed. Walked by
/// hand because `io::Error::source` skips the error it wraps.
fn tls_refusal(error: &reqwest::Error) -> Option<&rustls::Error> {
    use std::error::Error;

    let mut current: Option<&(dyn Error + 'static)> = Some(error);
    while let Some(error) = current {
        if let Some(refusal) = error.downcast_ref::<rustls::Error>() {
            return Some(refusal);
        }
        current = match error.downcast_ref::<std::io::Error>() {
            Some(io) => io.get_ref().map(|inner| inner as &(dyn Error + 'static)),
            None => error.source(),
        };
    }
    None
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
    use daruma_core::embed::Event;
    use daruma_domain::{Actor, NewTask};

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

    /// A self-signed certificate of the kind `daruma-server` generates.
    async fn certificate() -> daruma_discovery::CertBundle {
        let dir = std::env::temp_dir().join(format!("daruma-remote-{}", DeviceId::new()));
        let bundle = daruma_discovery::CertBundle::load_or_generate(&dir, "daruma.test")
            .await
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        bundle
    }

    /// A TLS server behind `bundle` that records every request head it decrypts
    /// and answers it with `reply`; returns its base URL and the record.
    async fn tls_server(
        bundle: daruma_discovery::CertBundle,
        reply: impl Fn(&str) -> String + Send + 'static,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let config = bundle.into_tls_config().unwrap().server_config;
        let acceptor = tokio_rustls::TlsAcceptor::from(config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        let heads = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = heads.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let Ok(mut stream) = acceptor.accept(tcp).await else {
                    continue;
                };
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk).await {
                        Ok(n) if n > 0 => request.extend_from_slice(&chunk[..n]),
                        _ => break,
                    }
                }
                let head = String::from_utf8_lossy(&request).into_owned();
                let _ = stream.write_all(reply(&head).as_bytes()).await;
                let _ = stream.shutdown().await;
                recorded.lock().unwrap().push(head);
            }
        });
        (url, heads)
    }

    /// What the daruma API answers on each endpoint the sink uses.
    fn api_reply(head: &str) -> String {
        let body = if head.starts_with("GET /v1/events?") {
            "[]"
        } else if head.starts_with("GET /v1/devices ") {
            r#"{"current_device_id":null,"devices":[]}"#
        } else {
            "null"
        };
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn envelope() -> EventEnvelope {
        EventEnvelope::new(
            Actor::user(),
            Event::TaskCreated {
                task: NewTask::new("t"),
            },
        )
    }

    fn pinned(bundle: &daruma_discovery::CertBundle) -> ServerTrust {
        ServerTrust::Pinned {
            fingerprint: format!("sha256:{}", bundle.fingerprint),
        }
    }

    fn paired(server_url: &str, tls_fingerprint: Option<&str>) -> PairedCredentials {
        PairedCredentials {
            server_url: server_url.to_owned(),
            token: "token".to_owned(),
            tls_fingerprint: tls_fingerprint.map(str::to_owned),
        }
    }

    #[test]
    fn pin_applies_to_the_paired_origin_only() {
        let credentials = paired("https://daruma.local:8443", Some("sha256:ab"));
        let trust = |url| server_trust(url, Some(&credentials));
        let pin = ServerTrust::Pinned {
            fingerprint: "sha256:ab".to_owned(),
        };

        assert_eq!(trust("https://daruma.local:8443"), pin);
        assert_eq!(trust("https://DARUMA.local:8443/"), pin);
        assert_eq!(trust("https://other.local:8443"), ServerTrust::System);
        assert_eq!(trust("https://daruma.local:9443"), ServerTrust::System);
        assert_eq!(trust("https://daruma.local"), ServerTrust::System);
        assert_eq!(trust("http://daruma.local:8443"), ServerTrust::System);
        assert_eq!(trust("not a url"), ServerTrust::System);
    }

    #[test]
    fn pin_compares_default_ports_as_ports() {
        let credentials = paired("https://daruma.local:443", Some("sha256:ab"));
        assert!(matches!(
            server_trust("https://daruma.local", Some(&credentials)),
            ServerTrust::Pinned { .. }
        ));
    }

    #[test]
    fn pairing_without_a_stored_fingerprint_is_not_pinned() {
        let credentials: PairedCredentials =
            serde_json::from_str(r#"{"server_url":"https://h:8443","token":"t"}"#).unwrap();
        assert_eq!(credentials.tls_fingerprint, None);
        assert_eq!(
            server_trust("https://h:8443", Some(&credentials)),
            ServerTrust::PairedWithoutPin
        );
        assert_eq!(
            server_trust("https://other:8443", Some(&credentials)),
            ServerTrust::System
        );
        assert_eq!(server_trust("https://h:8443", None), ServerTrust::System);
    }

    #[test]
    fn unreadable_stored_fingerprint_does_not_fall_back_to_another_trust() {
        for fingerprint in ["", "sha256:", "sha256:zz", "sha256:abcd"] {
            let trust = ServerTrust::Pinned {
                fingerprint: fingerprint.to_owned(),
            };
            let error = HttpReplicaSink::with_trust("https://h:8443", "token", trust)
                .err()
                .unwrap_or_else(|| panic!("{fingerprint:?} must be refused"));
            assert!(error.to_string().contains("tls_fingerprint"), "{error}");
        }
    }

    #[tokio::test]
    async fn pinned_certificate_serves_every_endpoint() {
        let bundle = certificate().await;
        let trust = pinned(&bundle);
        let (url, heads) = tls_server(bundle, api_reply).await;
        let sink = HttpReplicaSink::with_trust(&url, "pair-token", trust).unwrap();

        assert!(sink.fetch_events(0, 10).await.unwrap().is_empty());
        assert!(sink.fetch_snapshot().await.unwrap().is_none());
        assert!(matches!(
            sink.push(envelope()).await.unwrap(),
            PushOutcome::Accepted
        ));
        assert!(sink.list_devices().await.unwrap().devices.is_empty());
        sink.revoke_device(DeviceId::new()).await.unwrap();

        let heads = heads.lock().unwrap();
        let lines: Vec<&str> = heads.iter().filter_map(|h| h.lines().next()).collect();
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert!(lines[0].starts_with("GET /v1/events?since=0"), "{lines:?}");
        assert!(
            lines[1].starts_with("GET /v1/events/snapshot "),
            "{lines:?}"
        );
        assert!(
            lines[2].starts_with("POST /v1/events/replica "),
            "{lines:?}"
        );
        assert!(lines[3].starts_with("GET /v1/devices "), "{lines:?}");
        assert!(lines[4].contains("/revoke "), "{lines:?}");
        for head in heads.iter() {
            assert!(head.contains("pair-token"), "{head}");
        }
    }

    /// Every way the sink reaches the server, so none of them can skip the pin.
    async fn refusals(sink: &HttpReplicaSink) -> Vec<String> {
        vec![
            sink.fetch_events(0, 10).await.err().unwrap().to_string(),
            sink.fetch_snapshot().await.err().unwrap().to_string(),
            sink.push(envelope()).await.err().unwrap().to_string(),
            sink.list_devices().await.err().unwrap().to_string(),
            sink.revoke_device(DeviceId::new())
                .await
                .err()
                .unwrap()
                .to_string(),
        ]
    }

    #[tokio::test]
    async fn swapped_certificate_is_refused_before_the_token_is_sent() {
        let verified = certificate().await;
        let trust = pinned(&verified);
        let impostor = certificate().await;
        let impostor_fingerprint = impostor.fingerprint.clone();
        let (url, heads) = tls_server(impostor, api_reply).await;
        let sink = HttpReplicaSink::with_trust(&url, "pair-token", trust).unwrap();

        for refusal in refusals(&sink).await {
            assert!(refusal.contains("TLS fingerprint mismatch"), "{refusal}");
            assert!(
                refusal.contains(&format!("expected sha256:{}", verified.fingerprint)),
                "{refusal}"
            );
            assert!(
                refusal.contains(&format!("got sha256:{impostor_fingerprint}")),
                "{refusal}"
            );
            assert!(refusal.contains("`daruma pair`"), "{refusal}");
        }
        assert!(heads.lock().unwrap().is_empty(), "the token reached it");
    }

    #[tokio::test]
    async fn pairing_without_a_pin_does_not_accept_an_untrusted_server() {
        let (url, heads) = tls_server(certificate().await, api_reply).await;
        let sink =
            HttpReplicaSink::with_trust(&url, "pair-token", ServerTrust::PairedWithoutPin).unwrap();

        for refusal in refusals(&sink).await {
            assert!(refusal.contains("TLS refused"), "{refusal}");
            assert!(refusal.contains("`daruma pair`"), "{refusal}");
        }
        assert!(heads.lock().unwrap().is_empty(), "the token reached it");
    }

    #[tokio::test]
    async fn another_server_gets_no_trust_from_the_pin() {
        let bundle = certificate().await;
        let credentials = paired(
            "https://127.0.0.1:1",
            Some(&format!("sha256:{}", bundle.fingerprint)),
        );
        let (url, heads) = tls_server(bundle, api_reply).await;
        let trust = server_trust(&url, Some(&credentials));
        assert_eq!(trust, ServerTrust::System);
        let sink = HttpReplicaSink::with_trust(&url, "pair-token", trust).unwrap();

        for refusal in refusals(&sink).await {
            assert!(refusal.contains("TLS refused"), "{refusal}");
            assert!(!refusal.contains("`daruma pair`"), "{refusal}");
        }
        assert!(heads.lock().unwrap().is_empty(), "the token reached it");
    }

    #[tokio::test]
    async fn pinned_client_does_not_follow_a_redirect() {
        let bundle = certificate().await;
        let trust = pinned(&bundle);
        let (target, target_heads) = tls_server(bundle.clone(), api_reply).await;
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nlocation: {target}/v1/events\r\n\
             content-length: 0\r\nconnection: close\r\n\r\n"
        );
        let (url, _) = tls_server(bundle, move |_| redirect.clone()).await;
        let sink = HttpReplicaSink::with_trust(&url, "pair-token", trust).unwrap();

        let error = sink.fetch_events(0, 10).await.err().unwrap();
        assert!(error.to_string().contains("302"), "{error}");
        assert!(target_heads.lock().unwrap().is_empty());
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
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let outbox = crate::outbox::Outbox::new(db);
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
