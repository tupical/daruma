//! HMAC-SHA256 signing for webhook payloads.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Compute the lowercase hex HMAC-SHA256 of `body` keyed by `secret`.
///
/// This is the value placed in the `X-Taskagent-Signature` header. The
/// receiver re-runs the same hash over the raw request body and compares
/// in constant time.
pub fn sign_body_hex(secret: &str, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_deterministic() {
        let a = sign_body_hex("secret", b"hello");
        let b = sign_body_hex("secret", b"hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn signature_changes_with_key_or_body() {
        let base = sign_body_hex("secret", b"hello");
        assert_ne!(base, sign_body_hex("other", b"hello"));
        assert_ne!(base, sign_body_hex("secret", b"world"));
    }
}
