use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::pairing::FleetMessage;

type HmacSha256 = Hmac<Sha256>;

/// Sign a request body with the fleet HMAC token.
/// Returns the lowercase hex digest of HMAC-SHA256(token, body).
pub fn sign_envelope(token: &[u8], body: &[u8]) -> Result<String> {
    let mut mac = HmacSha256::new_from_slice(token)?;
    mac.update(body);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// Verify the HMAC header against the body, parse the body as a `FleetMessage`,
/// and return it. Constant-time comparison on the digest.
pub fn verify_envelope(token: &[u8], body: &[u8], header: &str) -> Result<FleetMessage> {
    let mut mac = HmacSha256::new_from_slice(token)?;
    mac.update(body);
    let expected = hex::encode(mac.finalize().into_bytes());
    if !constant_time_eq(expected.as_bytes(), header.as_bytes()) {
        return Err(anyhow!("envelope hmac mismatch"));
    }
    serde_json::from_slice::<FleetMessage>(body).map_err(Into::into)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
