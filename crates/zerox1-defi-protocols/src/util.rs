//! Shared helpers: Anchor instruction discriminator, ATA derivation.

use sha2::{Digest, Sha256};
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;

/// Compute the 8-byte Anchor instruction discriminator for a given
/// `<namespace>:<ix_name>` pair.
///
/// Anchor uses the first 8 bytes of `sha256(namespace_colon_ix_name)`.
/// Standard namespaces are `"global"` for top-level instructions and
/// `"state"` for state-method instructions.
///
/// Example:
/// ```
/// use zerox1_defi_protocols::util::anchor_discriminator;
/// let d = anchor_discriminator("global", "deposit");
/// assert_eq!(d.len(), 8);
/// ```
pub fn anchor_discriminator(namespace: &str, ix_name: &str) -> [u8; 8] {
    let preimage = format!("{namespace}:{ix_name}");
    let hash = Sha256::digest(preimage.as_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&hash[..8]);
    out
}

/// Derive the associated token account for a given owner + mint.
pub fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    get_associated_token_address(owner, mint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminator_is_eight_bytes() {
        let d = anchor_discriminator("global", "deposit");
        assert_eq!(d.len(), 8);
    }

    #[test]
    fn discriminator_is_deterministic() {
        let a = anchor_discriminator("global", "withdraw");
        let b = anchor_discriminator("global", "withdraw");
        assert_eq!(a, b);
    }

    #[test]
    fn discriminator_distinguishes_names() {
        assert_ne!(
            anchor_discriminator("global", "deposit"),
            anchor_discriminator("global", "withdraw"),
        );
    }
}
