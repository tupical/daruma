//! mDNS service advertisement for `_taskagent._tcp.local.`
//!
//! Wraps [`mdns_sd`] to broadcast the server's presence on the local network
//! with TXT records that carry enough metadata for a client to initiate
//! pairing without prior configuration.
//!
//! ## TXT record keys
//!
//! | Key               | Example value         | Description                                |
//! |-------------------|-----------------------|--------------------------------------------|
//! | `version`         | `0.2.0`               | Server `CARGO_PKG_VERSION`                 |
//! | `tls_fingerprint` | `sha256:<64 hex>`     | SHA-256 of the DER-encoded TLS certificate |
//!
//! `host_id` was intentionally removed: it served no protocol purpose and
//! acted as a stable tracking beacon on the LAN.

use std::collections::HashMap;

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};

/// Service type advertised on the LAN.
pub const SERVICE_TYPE: &str = "_taskagent._tcp.local.";

/// Handle to an active mDNS advertisement.  Drop to stop advertising.
pub struct MdnsAdvertiser {
    daemon: ServiceDaemon,
    instance_name: String,
}

impl MdnsAdvertiser {
    /// Start advertising `_taskagent._tcp` on the local network.
    ///
    /// # Parameters
    ///
    /// - `instance_name`: Human-readable service label (e.g. hostname).
    /// - `port`: The TLS port clients should connect to.
    /// - `tls_fingerprint`: `sha256:<hex>` fingerprint string.
    /// - `version`: The server's cargo package version string.
    pub fn start(
        instance_name: &str,
        port: u16,
        tls_fingerprint: &str,
        version: &str,
    ) -> Result<Self> {
        let daemon = ServiceDaemon::new().context("create mDNS daemon")?;

        let mut properties: HashMap<String, String> = HashMap::new();
        properties.insert("version".into(), version.into());
        properties.insert("tls_fingerprint".into(), tls_fingerprint.into());

        let info = ServiceInfo::new(
            SERVICE_TYPE,
            instance_name,
            &format!("{instance_name}.local."),
            "",  // let mdns-sd resolve local IPs automatically
            port,
            properties,
        )
        .context("build mDNS ServiceInfo")?;

        daemon.register(info).context("register mDNS service")?;

        tracing::info!(
            service_type = SERVICE_TYPE,
            instance = instance_name,
            port,
            tls_fingerprint,
            "mDNS advertisement started"
        );

        Ok(Self {
            daemon,
            instance_name: instance_name.to_string(),
        })
    }
}

impl Drop for MdnsAdvertiser {
    fn drop(&mut self) {
        let fullname = format!("{}.{}", self.instance_name, SERVICE_TYPE);
        if let Err(e) = self.daemon.unregister(&fullname) {
            tracing::debug!(err = %e, "mDNS unregister on drop (ignored)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_info_builds_without_panic() {
        let mut props = HashMap::new();
        props.insert("version".into(), "0.2.0".into());
        props.insert("tls_fingerprint".into(), "sha256:abc123".into());

        let result = ServiceInfo::new(
            SERVICE_TYPE,
            "taskagent-test",
            "taskagent-test.local.",
            "",
            8443u16,
            props,
        );
        assert!(result.is_ok(), "ServiceInfo::new failed: {:?}", result.err());
    }
}
