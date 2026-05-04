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
            return Err(anyhow!("wallet keypair must be 64 bytes, got {}", bytes.len()));
        }
        let keypair = Keypair::from_bytes(&bytes).context("construct keypair")?;
        Ok(Self { keypair })
    }

    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    pub fn keypair(&self) -> &Keypair {
        &self.keypair
    }
}

/// A daemon's mandate — which programs it is allowed to sign for.
/// Each daemon constructs a `SigningWhitelist` once at boot and runs every
/// outbound transaction through `verify_tx` before signing.
pub struct SigningWhitelist {
    allowed: Vec<Pubkey>,
}

impl SigningWhitelist {
    pub fn new(allowed: Vec<Pubkey>) -> Self {
        Self { allowed }
    }

    pub fn verify_tx(&self, tx: &solana_sdk::transaction::Transaction) -> anyhow::Result<()> {
        for ix in tx.message.instructions.iter() {
            let program_id = tx.message.account_keys.get(ix.program_id_index as usize)
                .ok_or_else(|| anyhow::anyhow!("malformed instruction: program_id_index out of bounds"))?;
            if !self.allowed.contains(program_id) {
                return Err(anyhow::anyhow!(
                    "signing whitelist violation: program {} not allowed for this daemon",
                    program_id
                ));
            }
        }
        Ok(())
    }
}

impl Wallet {
    /// Sign a transaction only if every instruction targets a whitelisted program.
    pub fn sign_with_whitelist(
        &self,
        tx: &mut solana_sdk::transaction::Transaction,
        whitelist: &SigningWhitelist,
        recent_blockhash: solana_sdk::hash::Hash,
    ) -> anyhow::Result<()> {
        whitelist.verify_tx(tx)?;
        tx.try_sign(&[&self.keypair], recent_blockhash)?;
        Ok(())
    }
}

#[cfg(test)]
mod whitelist_tests {
    use super::*;
    use solana_sdk::{
        instruction::{AccountMeta, Instruction},
        message::Message,
        signature::{Keypair, Signer},
        transaction::Transaction,
    };

    #[test]
    fn rejects_unwhitelisted_program() {
        let payer = Keypair::new();
        let allowed = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let ix = Instruction::new_with_bytes(other, &[], vec![AccountMeta::new(payer.pubkey(), true)]);
        let msg = Message::new(&[ix], Some(&payer.pubkey()));
        let tx = Transaction::new_unsigned(msg);
        let wl = SigningWhitelist::new(vec![allowed]);
        assert!(wl.verify_tx(&tx).is_err());
    }

    #[test]
    fn accepts_whitelisted_program() {
        let payer = Keypair::new();
        let allowed = Pubkey::new_unique();
        let ix = Instruction::new_with_bytes(allowed, &[], vec![AccountMeta::new(payer.pubkey(), true)]);
        let msg = Message::new(&[ix], Some(&payer.pubkey()));
        let tx = Transaction::new_unsigned(msg);
        let wl = SigningWhitelist::new(vec![allowed]);
        assert!(wl.verify_tx(&tx).is_ok());
    }
}
