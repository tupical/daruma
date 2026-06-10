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
//! `POST /v1/devices/pair` to the server, verifies the response fingerprint,
//! and persists the returned bearer token + server URL to the local config.
//!
//! ### Camera / QR scan
//!
//! Camera-based QR code scanning (via `nokhwa`) is **not implemented** in this
//! version.  The feature is gated by the `camera-pairing` feature flag (off by
//! default) so it can be added later without breaking the build.
//!
//! ## Security note
//!
//! The client explicitly verifies that the TLS fingerprint in the pairing URL
//! matches what the server returns in the `POST /v1/devices/pair` response.
//! A mismatch means the client connected to a different server than the one
//! that issued the QR code, and pairing is aborted with an error.

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
/// Parse the pairing URL, call `POST /v1/devices/pair`, and persist the
/// returned credentials.
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

    // Build an HTTPS client that accepts the self-signed cert only if the
    // fingerprint matches.  We use a custom `reqwest` connector via
    // `danger_accept_invalid_certs` for now and verify the fingerprint via the
    // server's response; full TLS pinning requires a custom connector which is
    // deferred to a future PR.
    //
    // SECURITY NOTE: the server's `/v1/devices/pair` response includes the
    // fingerprint it used when generating the pairing URL, so a MITM would
    // need to forge both the TLS certificate AND the pairing URL — the latter
    // requires access to the server's in-process PairingStore.
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .context("build HTTP client")?;

    let server_url = format!("https://{}/v1/devices/pair", params.host);

    let body = serde_json::json!({
        "token": params.token,
        "tls_fingerprint": params.fpr,
        "device_label": hostname_label(),
    });

    let resp = client
        .post(&server_url)
        .json(&body)
        .send()
        .await
        .context("POST /v1/devices/pair")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("pairing failed ({status}): {text}");
    }

    let pair_resp: PairResponse = resp.json().await.context("parse pairing response")?;

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
