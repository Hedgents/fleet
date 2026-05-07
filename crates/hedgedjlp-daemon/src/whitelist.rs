//! Program-ID whitelist for hedgedjlp-daemon.
//!
//! For M3 the whitelist is intentionally **empty** — the daemon doesn't
//! sign any chain-touching ixns yet. M6 adds the JLP buy leg (Jupiter
//! swap program + ATA + token program + system + compute budget). M8
//! adds the Jupiter Perps hedge leg (perps program + perps position
//! manager). Until then, `SigningWhitelist::verify_ixns` (audit-fix I1)
//! refuses every ixn slice — which is correct, because nothing in M3
//! ever calls into the wallet.
//!
//! Mirrors the `kamino::whitelist_program_ids()` shape used by
//! stable-yield-daemon so M6/M8 can drop ids in without touching the
//! signature.

use solana_sdk::pubkey::Pubkey;

/// Allowed Solana program ids for the hedgedjlp wallet. Empty in M3;
/// populated in M6 (Jupiter swap + token plumbing) and M8 (Jupiter
/// Perps).
pub fn whitelist_program_ids() -> Vec<Pubkey> {
    Vec::new()
}
