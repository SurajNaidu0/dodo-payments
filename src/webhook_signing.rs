//! Webhook signatures.
//!
//! Header: `Dodo-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256>`
//! Signed message: `"<t>.<raw request body>"`, keyed with the endpoint secret.
//!
//! Receivers recompute the HMAC over the raw body, compare in constant time,
//! and reject timestamps older than a few minutes (replay protection).

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub const HEADER: &str = "Dodo-Signature";

pub fn sign(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

pub fn header_value(secret: &str, timestamp: i64, body: &[u8]) -> String {
    format!("t={timestamp},v1={}", sign(secret, timestamp, body))
}

/// What a receiver would run. Used by our tests.
pub fn verify(secret: &str, header: &str, body: &[u8], now: i64, tolerance_secs: i64) -> bool {
    let mut timestamp = None;
    let mut signature = None;
    for part in header.split(',') {
        match part.split_once('=') {
            Some(("t", v)) => timestamp = v.parse::<i64>().ok(),
            Some(("v1", v)) => signature = hex::decode(v).ok(),
            _ => {}
        }
    }
    let (Some(timestamp), Some(signature)) = (timestamp, signature) else {
        return false;
    };
    if (now - timestamp).abs() > tolerance_secs {
        return false;
    }
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_signature_verifies() {
        let header = header_value("whsec_test", 1_000, b"{\"a\":1}");
        assert!(verify("whsec_test", &header, b"{\"a\":1}", 1_010, 300));
    }

    #[test]
    fn tampered_body_wrong_secret_or_old_timestamp_fail() {
        let header = header_value("whsec_test", 1_000, b"{\"a\":1}");
        assert!(!verify("whsec_test", &header, b"{\"a\":2}", 1_000, 300));
        assert!(!verify("whsec_other", &header, b"{\"a\":1}", 1_000, 300));
        assert!(!verify("whsec_test", &header, b"{\"a\":1}", 2_000, 300));
    }
}
