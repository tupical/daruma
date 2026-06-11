//! Self-signed TLS certificate lifecycle.
//!
//! On first call to [`CertBundle::load_or_generate`] a fresh ECDSA-P256
//! certificate is generated with `rcgen`, written to `<data_dir>/tls/` as
//! `cert.pem` + `key.pem`, and returned.  On subsequent calls the PEM files
//! are loaded from disk — the fingerprint is therefore stable across restarts.
//!
//! The SHA-256 fingerprint of the DER-encoded certificate is surfaced as a
//! hex string for embedding in QR codes and mDNS TXT records.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use sha2::{Digest, Sha256};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::ServerConfig;

/// PEM + DER representation of the server certificate and its private key.
#[derive(Clone)]
pub struct CertBundle {
    /// DER bytes of the certificate (used for fingerprint computation and TLS).
    pub cert_der: Vec<u8>,
    /// PEM of the certificate (for persistence).
    pub cert_pem: String,
    /// PEM of the private key (for persistence).
    pub key_pem: String,
    /// Hex-encoded SHA-256 digest of `cert_der`, e.g. `"ab12cd…"` (64 hex chars).
    pub fingerprint: String,
}

/// Rustls [`ServerConfig`] built from a [`CertBundle`].
pub struct TlsConfig {
    pub server_config: std::sync::Arc<ServerConfig>,
    pub fingerprint: String,
}

impl CertBundle {
    /// Load an existing certificate from `<data_dir>/tls/` or generate a new
    /// one, persist it, and return the bundle.
    pub async fn load_or_generate(data_dir: &Path, hostname: &str) -> Result<Self> {
        let tls_dir = data_dir.join("tls");
        tokio::fs::create_dir_all(&tls_dir)
            .await
            .context("create tls dir")?;

        let cert_path = tls_dir.join("cert.pem");
        let key_path = tls_dir.join("key.pem");

        if cert_path.exists() && key_path.exists() {
            match Self::load_from_disk(&cert_path, &key_path).await {
                Ok(bundle) => {
                    tracing::info!(
                        fingerprint = %bundle.fingerprint,
                        "loaded existing TLS certificate"
                    );
                    return Ok(bundle);
                }
                Err(e) => {
                    tracing::warn!(err = %e, "failed to load existing TLS cert, regenerating");
                }
            }
        }

        let bundle = Self::generate(hostname).context("generate TLS cert")?;
        bundle.persist(&cert_path, &key_path).await?;

        tracing::info!(
            fingerprint = %bundle.fingerprint,
            "generated new self-signed TLS certificate"
        );
        Ok(bundle)
    }

    /// Generate a fresh self-signed ECDSA-P256 certificate valid for 10 years.
    fn generate(hostname: &str) -> Result<Self> {
        let key_pair = KeyPair::generate().context("generate key pair")?;

        let mut params = CertificateParams::default();
        params.subject_alt_names = vec![SanType::DnsName(
            hostname.to_string().try_into().context("SAN hostname")?,
        )];
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, format!("taskagent@{hostname}"));
        params.distinguished_name = dn;
        // 10-year validity
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2034, 1, 1);

        let cert = params.self_signed(&key_pair).context("self-sign cert")?;
        let cert_der = cert.der().to_vec();
        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();
        let fingerprint = sha256_hex(&cert_der);

        Ok(CertBundle {
            cert_der,
            cert_pem,
            key_pem,
            fingerprint,
        })
    }

    async fn load_from_disk(cert_path: &PathBuf, key_path: &PathBuf) -> Result<Self> {
        let cert_pem = tokio::fs::read_to_string(cert_path)
            .await
            .context("read cert.pem")?;
        let key_pem = tokio::fs::read_to_string(key_path)
            .await
            .context("read key.pem")?;

        // Parse DER from PEM to compute fingerprint.
        let cert_der = pem_to_der(&cert_pem).context("parse cert PEM")?;
        let fingerprint = sha256_hex(&cert_der);

        Ok(CertBundle {
            cert_der,
            cert_pem,
            key_pem,
            fingerprint,
        })
    }

    async fn persist(&self, cert_path: &PathBuf, key_path: &PathBuf) -> Result<()> {
        tokio::fs::write(cert_path, &self.cert_pem)
            .await
            .context("write cert.pem")?;
        tokio::fs::write(key_path, &self.key_pem)
            .await
            .context("write key.pem")?;

        // Restrict key permissions on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = tokio::fs::metadata(key_path).await {
                let mut perms = meta.permissions();
                perms.set_mode(0o600);
                let _ = tokio::fs::set_permissions(key_path, perms).await;
            }
        }

        Ok(())
    }

    /// Build a [`TlsConfig`] from this bundle.
    pub fn into_tls_config(self) -> Result<TlsConfig> {
        let fingerprint = self.fingerprint.clone();

        let cert_der = CertificateDer::from(self.cert_der);
        let key_der = pem_to_pkcs8_der(&self.key_pem).context("parse key PEM")?;
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));

        let mut server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], private_key)
            .context("build rustls ServerConfig")?;

        server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

        Ok(TlsConfig {
            server_config: std::sync::Arc::new(server_config),
            fingerprint,
        })
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// SHA-256 digest as a lowercase hex string.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Extract the first PEM block and return its DER bytes.
fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    // Simple PEM decode: strip header/footer, base64-decode the body.
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .context("base64 decode PEM body")
}

/// Extract PKCS#8 DER from a PEM key.
fn pem_to_pkcs8_der(pem: &str) -> Result<Vec<u8>> {
    pem_to_der(pem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn generate_and_reload() {
        let tmp = TempDir::new().unwrap();
        let bundle1 = CertBundle::load_or_generate(tmp.path(), "localhost")
            .await
            .unwrap();
        let bundle2 = CertBundle::load_or_generate(tmp.path(), "localhost")
            .await
            .unwrap();
        assert_eq!(bundle1.fingerprint, bundle2.fingerprint);
        assert_eq!(bundle1.fingerprint.len(), 64); // SHA-256 hex
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let data = b"test certificate data";
        let fp1 = sha256_hex(data);
        let fp2 = sha256_hex(data);
        assert_eq!(fp1, fp2);
        assert_eq!(fp1.len(), 64);
    }
}
