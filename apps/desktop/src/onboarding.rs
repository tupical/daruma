//! LAN discovery and pairing onboarding for the desktop CLI.
//!
//! ## Subcommands
//!
//! ### `taskagent discover`
//!
//! Listens for `_taskagent._tcp.local.` mDNS announcements for up to 5 seconds
//! (or `--timeout <secs>`) and prints a table of discovered hosts with their
//! TLS fingerprints.  The list can be used to choose a host for `pair`.
//!
//! ### `taskagent pair <url>`
//!
//! Accepts a `taskagent://pair?host=…&token=…&fpr=sha256:…` URL (paste from
//! QR code or copy from the server's `/v1/devices/pair/ticket` output), sends
//! `POST /v1/devices/pair` to the server over TLS, and persists the returned
//! bearer token + server URL to the local config.
//!
//! ### Camera / QR scan
//!
//! Camera-based QR code scanning (via `nokhwa`) is **not implemented** in this
//! version.  The feature is gated by the `camera-pairing` feature flag (off by
//! default) so it can be added later without breaking the build.
//!
//! ## Security model
//!
//! The server uses a self-signed TLS certificate.  Standard CA verification
//! would reject it, so this client implements *fingerprint pinning* instead:
//!
//! 1. The pairing URL carries `fpr=sha256:<hex>` — the expected SHA-256 digest
//!    of the server's leaf certificate DER bytes.
//! 2. A custom `rustls::client::ServerCertVerifier` computes the digest of the
//!    actual leaf cert that arrives in the TLS handshake and compares it with
//!    the expected value using a constant-time equality check.
//! 3. If the digests differ the TLS handshake is aborted before any HTTP bytes
//!    are exchanged; a MITM cannot forge the cert even if they intercept DNS /
//!    mDNS because they do not possess the server's private key.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

// ── discover ──────────────────────────────────────────────────────────────────

/// `taskagent discover [--timeout <secs>]`
///
/// Scans the LAN for `_taskagent._tcp.local.` services and prints a summary
/// table.
pub async fn cmd_discover(args: &[String]) -> Result<()> {
    let timeout_secs: u64 = parse_timeout(args).unwrap_or(5);

    println!("Scanning for taskagent servers on the LAN ({timeout_secs}s)…");

    let discovered = scan_mdns(timeout_secs).await;

    if discovered.is_empty() {
        println!("No taskagent servers found.");
        println!("Hint: ensure the server is running and mDNS is not blocked by your firewall.");
        return Ok(());
    }

    println!("\n{:<30} {:<25} {}", "Instance", "Host:Port", "TLS fingerprint (sha256:)");
    println!("{}", "-".repeat(100));
    for h in &discovered {
        println!(
            "{:<30} {:<25} sha256:{}",
            truncate(&h.instance, 29),
            h.addr,
            truncate(&h.fingerprint, 20),
        );
    }
    println!("\nUse `taskagent pair <url>` to pair with a discovered server.");
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

/// Discovered host from mDNS browse.
struct DiscoveredHost {
    instance: String,
    addr: String,
    fingerprint: String,
}

/// Browse `_taskagent._tcp.local.` for `timeout_secs` and collect results.
async fn scan_mdns(timeout_secs: u64) -> Vec<DiscoveredHost> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};

    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("mDNS browse unavailable: {e}");
            return vec![];
        }
    };

    let receiver = match daemon.browse(taskagent_discovery::mdns::SERVICE_TYPE) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mDNS browse failed: {e}");
            return vec![];
        }
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut hosts: Vec<DiscoveredHost> = Vec::new();

    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let instance = info.get_fullname().to_string();
                let port = info.get_port();
                let addr = info
                    .get_addresses()
                    .iter()
                    .next()
                    .map(|a| format!("{a}:{port}"))
                    .unwrap_or_else(|| format!("?:{port}"));
                let fpr = info
                    .get_properties()
                    .get("tls_fingerprint")
                    .map(|p| p.val_str().to_string())
                    .unwrap_or_else(|| "<unknown>".into());
                // Strip leading "sha256:" prefix if present for display.
                let fpr = fpr.trim_start_matches("sha256:").to_string();
                hosts.push(DiscoveredHost {
                    instance,
                    addr,
                    fingerprint: fpr,
                });
            }
            Ok(_) => {}
            Err(_) => break, // timeout or channel closed
        }
    }

    let _ = daemon.shutdown();
    hosts
}

fn parse_timeout(args: &[String]) -> Option<u64> {
    let pos = args.iter().position(|a| a == "--timeout" || a == "-t")?;
    args.get(pos + 1)?.parse().ok()
}

// ── pair ──────────────────────────────────────────────────────────────────────

/// `taskagent pair <taskagent://pair?…>`
///
/// Parse the pairing URL, call `POST /v1/devices/pair` over a TLS connection
/// whose leaf-certificate fingerprint is pinned to the value in the URL, and
/// persist the returned credentials.
pub async fn cmd_pair(args: &[String]) -> Result<()> {
    let url_str = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("usage: taskagent pair <taskagent://pair?host=…&token=…&fpr=sha256:…>"))?;

    // Camera pairing stub — off by default; can be unlocked with feature flag.
    #[cfg(feature = "camera-pairing")]
    {
        eprintln!("Note: camera-pairing feature is enabled but not yet implemented.");
    }

    let params = parse_pairing_url(url_str)
        .with_context(|| format!("invalid pairing URL: {url_str}"))?;

    println!("Pairing with {}…", params.host);
    println!("  Expected TLS fingerprint: {}", params.fpr);

    // Strip "sha256:" prefix to get the raw hex string expected.
    let expected_hex = params
        .fpr
        .strip_prefix("sha256:")
        .unwrap_or(&params.fpr)
        .to_string();

    // Build and send the pairing request over a fingerprint-pinned TLS channel.
    let pair_resp = post_pair_pinned(&params.host, &params.token, &expected_hex)
        .await
        .context("POST /v1/devices/pair")?;

    // Persist credentials.
    persist_credentials(&params.host, &pair_resp.access_token)
        .await
        .context("persist credentials")?;

    println!("Paired successfully!");
    println!("  Server URL  : https://{}", params.host);
    println!("  Token prefix: {}", pair_resp.token_prefix);
    println!();
    println!("Credentials written to the local config.");
    println!("Run `taskagent sync` to pull tasks from the server.");

    Ok(())
}

// ── TLS fingerprint-pinned HTTP client ────────────────────────────────────────

/// Send `POST /v1/devices/pair` over a rustls connection that pins the server's
/// leaf certificate to `expected_sha256_hex`.
///
/// The custom verifier computes SHA-256 of the raw leaf DER during the TLS
/// handshake and aborts with an error if it does not match `expected_sha256_hex`
/// (constant-time comparison).  This is the correct way to handle self-signed
/// certificates: CA chain verification is replaced by fingerprint pinning.
async fn post_pair_pinned(
    host: &str,
    token: &str,
    expected_sha256_hex: &str,
) -> Result<PairResponse> {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsConnector;

    // Build a rustls ClientConfig that skips CA verification and instead
    // delegates to our fingerprint verifier.
    let verifier = Arc::new(FingerprintVerifier::new(expected_sha256_hex)?);
    let tls_cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(tls_cfg));

    // Resolve host and port.
    let (hostname, port) = split_host_port(host)?;
    let addr_str = format!("{hostname}:{port}");
    let addrs: Vec<_> = tokio::net::lookup_host(&addr_str)
        .await
        .with_context(|| format!("DNS lookup failed for {addr_str}"))?
        .collect();
    let addr = addrs
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no address for {addr_str}"))?;

    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .with_context(|| format!("TCP connect to {addr}"))?;

    let server_name = rustls::pki_types::ServerName::try_from(hostname.to_string())
        .map_err(|e| anyhow::anyhow!("invalid server name {hostname}: {e}"))?;

    let tls = connector
        .connect(server_name, tcp)
        .await
        .context("TLS handshake (fingerprint mismatch aborts here)")?;

    // Build a minimal HTTP/1.1 POST request by hand — avoids pulling in a
    // second HTTP client stack just for this one call.
    let body_bytes = serde_json::to_vec(&serde_json::json!({
        "token": token,
        "device_label": hostname_label(),
    }))?;
    let request = format!(
        "POST /v1/devices/pair HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body_bytes.len()
    );

    let mut tls = tls;
    tls.write_all(request.as_bytes()).await?;
    tls.write_all(&body_bytes).await?;
    tls.flush().await?;

    // Read the full response.
    let mut raw = Vec::new();
    tls.read_to_end(&mut raw).await?;
    let response = String::from_utf8_lossy(&raw);

    // Split status line from body.
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("malformed HTTP response"))?;

    let status_line = head.lines().next().unwrap_or("");
    let status_code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("cannot parse HTTP status from: {status_line}"))?;

    if !(200..300).contains(&status_code) {
        bail!("pairing failed (HTTP {status_code}): {body}");
    }

    let pair_resp: PairResponse =
        serde_json::from_str(body).context("parse pairing response JSON")?;
    Ok(pair_resp)
}

/// Parse `"host:port"` or `"host"` (defaulting to port 8443).
fn split_host_port(host: &str) -> Result<(&str, u16)> {
    if let Some((h, p)) = host.rsplit_once(':') {
        let port: u16 = p
            .parse()
            .with_context(|| format!("invalid port in {host}"))?;
        Ok((h, port))
    } else {
        Ok((host, 8443))
    }
}

// ── Custom rustls ServerCertVerifier — fingerprint pinning ────────────────────

/// A `rustls` [`ServerCertVerifier`] that accepts **any** certificate whose
/// SHA-256 digest of the raw DER bytes matches `expected_hex`.
///
/// All other certificates are rejected regardless of issuer or expiry.
/// The comparison uses `subtle::ConstantTimeEq` to prevent timing attacks.
#[derive(Debug)]
struct FingerprintVerifier {
    /// Lower-case hex SHA-256 of the expected leaf cert DER.
    expected: Vec<u8>,
}

impl FingerprintVerifier {
    fn new(expected_hex: &str) -> Result<Self> {
        let expected = hex::decode(expected_hex)
            .with_context(|| format!("invalid fingerprint hex: {expected_hex}"))?;
        if expected.len() != 32 {
            bail!("SHA-256 fingerprint must be 32 bytes (64 hex chars), got {}", expected.len());
        }
        Ok(Self { expected })
    }
}

impl rustls::client::danger::ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        use sha2::Digest;
        use subtle::ConstantTimeEq;

        let actual = sha2::Sha256::digest(end_entity.as_ref());

        if actual.ct_eq(&self.expected).into() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "TLS fingerprint mismatch: expected sha256:{}, got sha256:{}",
                hex::encode(&self.expected),
                hex::encode(actual),
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct PairingUrlParams {
    host: String,
    token: String,
    fpr: String,
}

fn parse_pairing_url(url: &str) -> Result<PairingUrlParams> {
    // Strip the scheme — accept both taskagent:// and https://
    let rest = url
        .strip_prefix("taskagent://pair?")
        .or_else(|| url.strip_prefix("taskagent://pair/?"))
        .ok_or_else(|| anyhow::anyhow!("URL must start with taskagent://pair?"))?;

    let mut host = None;
    let mut token = None;
    let mut fpr = None;

    for kv in rest.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            let decoded = percent_decode(v);
            match k {
                "host" => host = Some(decoded),
                "token" => token = Some(decoded),
                "fpr" => fpr = Some(decoded),
                _ => {}
            }
        }
    }

    Ok(PairingUrlParams {
        host: host.ok_or_else(|| anyhow::anyhow!("missing host"))?,
        token: token.ok_or_else(|| anyhow::anyhow!("missing token"))?,
        fpr: fpr.ok_or_else(|| anyhow::anyhow!("missing fpr"))?,
    })
}

/// Minimal percent-decoding for query-string values.
fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte as char);
                    i += 3;
                    continue;
                }
            }
        }
        if bytes[i] == b'+' {
            out.push(' ');
        } else {
            out.push(bytes[i] as char);
        }
        i += 1;
    }
    out
}

/// Minimal response shape from `POST /v1/devices/pair`.
#[derive(Deserialize)]
struct PairResponse {
    access_token: String,
    token_prefix: String,
    #[allow(dead_code)] // included in JSON for forward-compat; not used locally
    server_url: String,
}

/// Return a human-readable device label for the pairing request.
fn hostname_label() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
        .unwrap_or_else(|_| "desktop".to_string())
}

/// Persist `server_url` + `token` to the local config file used by `sync`.
///
/// This writes (or overwrites) `<data_dir>/paired.json` — the `sync` command
/// checks this file for credentials when `TASKAGENT_API_URL` / `TASKAGENT_TOKEN`
/// are not set.
async fn persist_credentials(host: &str, token: &str) -> Result<()> {
    let data_dir = crate::context::data_path();
    tokio::fs::create_dir_all(&data_dir)
        .await
        .context("create data dir")?;

    let config = serde_json::json!({
        "server_url": format!("https://{host}"),
        "token": token,
    });

    let path = data_dir.join("paired.json");
    tokio::fs::write(&path, serde_json::to_vec_pretty(&config)?)
        .await
        .context("write paired.json")?;

    // Restrict to owner-read-only on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = tokio::fs::set_permissions(&path, perms).await;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pairing_url_roundtrip() {
        let url = "taskagent://pair?host=192.168.1.5%3A8443&token=abc123&fpr=sha256%3Adeadbeef";
        let p = parse_pairing_url(url).unwrap();
        assert_eq!(p.host, "192.168.1.5:8443");
        assert_eq!(p.token, "abc123");
        assert_eq!(p.fpr, "sha256:deadbeef");
    }

    #[test]
    fn parse_pairing_url_missing_token() {
        let url = "taskagent://pair?host=x%3A8443&fpr=sha256%3Aabc";
        assert!(parse_pairing_url(url).is_err());
    }

    #[test]
    fn percent_decode_roundtrip() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("a%3Ab"), "a:b");
        assert_eq!(percent_decode("a+b"), "a b");
    }
}
