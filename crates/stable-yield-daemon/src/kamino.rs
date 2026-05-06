//! Kamino program-id whitelist for stable-yield-daemon.
//!
//! M3 stub: empty whitelist. M6 will populate this once the
//! lending-supply ixn lands so `SigningWhitelist::permit` only
//! allows the Kamino lending program (and any required CPIs).

use solana_sdk::pubkey::Pubkey;

/// Set of Solana program ids the stable-yield wallet is allowed to sign
/// instructions against. Empty for M3 — the daemon does not yet build
/// or submit any tx. M6 fills this in.
pub fn whitelist_program_ids() -> Vec<Pubkey> {
    Vec::new()
}
