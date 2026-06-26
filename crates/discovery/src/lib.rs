//! LAN discovery and device pairing for daruma.
//!
//! ## Responsibilities
//!
//! - **TLS**: generate a self-signed certificate on first run, persist it in
//!   the data directory, expose a DER-encoded SHA-256 fingerprint for
//!   out-of-band verification (§3.3.5 AC).
//! - **mDNS**: advertise `_daruma._tcp.local.` with TXT records
//!   `version`, `host_id`, `tls_fingerprint` (§3.3.5 §1).
//! - **Pairing**: single-use, TTL-5-min tokens stored in-process; the
//!   `/v1/devices/pair` endpoint consumes them (§3.3.5 §2).
//! - **QR**: encode a `daruma://pair?host=…&token=…&fpr=sha256:…` URL as
//!   a PNG (§3.3.5 §2).
//!
//! ## Security invariants
//!
//! - Pairing tokens are single-use and expire after 5 minutes.
//! - All token comparisons are constant-time (via [`subtle`]-equivalent logic
//!   using [`hmac`]).
//! - Tokens are **never** logged; tracing uses only their first 6 chars as a
//!   hint prefix.

pub mod mdns;
pub mod pairing;
pub mod qr;
pub mod tls;

pub use mdns::MdnsAdvertiser;
pub use pairing::{PairingStore, PairingTicket};
pub use tls::{CertBundle, TlsConfig};
