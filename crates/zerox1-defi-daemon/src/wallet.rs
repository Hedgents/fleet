use std::path::Path;

use anyhow::{anyhow, Context, Result};
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};

/// Wallet wrapper that owns a `Keypair` and exposes signing.
pub struct Wallet {
    keypair: Keypair,
}

impl Wallet {
    /// Load a Solana CLI–format keypair (JSON array of 64 bytes).
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read wallet keypair at {}", path.display()))?;
        let bytes: Vec<u8> = serde_json::from_str(&raw)
            .context("parse wallet keypair JSON (expected array of 64 bytes)")?;
        if bytes.len() != 64 {
            return Err(anyhow!(
                "wallet keypair must be 64 bytes, got {}",
                bytes.len()
            ));
        }
        let keypair =
            Keypair::try_from(&bytes[..]).map_err(|e| anyhow!("construct keypair: {e}"))?;
        Ok(Self { keypair })
    }

    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    pub fn keypair(&self) -> &Keypair {
        &self.keypair
    }
}
