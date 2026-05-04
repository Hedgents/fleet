use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::pairing::{FleetMessage, PairingState};

type HmacSha256 = Hmac<Sha256>;

pub fn sign_envelope(state: &PairingState, body: &[u8]) -> Result<String> {
    let token = state.fleet_token().ok_or_else(|| anyhow!("not paired"))?;
    let mut mac = HmacSha256::new_from_slice(token.as_bytes())?;
    mac.update(body);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

pub fn verify_envelope(state: &PairingState, body: &[u8], header: &str) -> Result<FleetMessage> {
    let token = state.fleet_token().ok_or_else(|| anyhow!("not paired"))?;
    let mut mac = HmacSha256::new_from_slice(token.as_bytes())?;
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
