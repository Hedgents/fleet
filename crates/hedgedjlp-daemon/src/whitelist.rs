//! Program-ID whitelist for hedgedjlp-daemon.
//!
//! M6 populates the whitelist for the JLP-buy leg via Jupiter Perps
//! `add_liquidity_2`. This covers:
//!
//!   - Jupiter Perpetuals (the `add_liquidity_2` ixn itself)
//!   - SPL Token (CPI'd by ATA-create + perps program for token transfers)
//!   - Associated Token Account (idempotent ATA creation for input + JLP)
//!   - System program (account creation paths inside ATA helpers)
//!   - Compute budget (set_compute_unit_limit + set_compute_unit_price
//!     prepended automatically by `RpcContext::build_signed`)
//!
//! M8 will extend this with the Jupiter Perps hedge-leg (`create_request`
//! / `execute_request`) — but for the buy path the same Jupiter Perps
//! program already covers it, so the whitelist won't grow.
//!
//! Mirrors the `kamino::whitelist_program_ids()` shape used by
//! stable-yield-daemon. `SigningWhitelist::verify_ixns` (audit-fix I1)
//! runs on every ixn slice before signing on BOTH the sim-only and submit
//! paths.

use solana_sdk::pubkey::Pubkey;

use zerox1_defi_protocols::constants::{
    ASSOCIATED_TOKEN_PROGRAM_ID, JUPITER_PERPETUALS_PROGRAM_ID, SYSTEM_PROGRAM_ID, TOKEN_PROGRAM_ID,
};

/// Allowed Solana program ids for the hedgedjlp wallet. Five programs
/// — exactly enough to cover the JLP-buy leg. The Jupiter Perps hedge
/// leg (M8) reuses `JUPITER_PERPETUALS_PROGRAM_ID`, so the whitelist
/// will not grow.
pub fn whitelist_program_ids() -> Vec<Pubkey> {
    vec![
        // Jupiter Perpetuals (add_liquidity_2 + future hedge-open ixns).
        JUPITER_PERPETUALS_PROGRAM_ID,
        // SPL Token (CPI'd by ATA-create + perps program for transfers).
        TOKEN_PROGRAM_ID,
        // Associated Token Account (idempotent ATA creation for the
        // user's USDC input ATA + JLP output ATA).
        ASSOCIATED_TOKEN_PROGRAM_ID,
        // System program (rent + account creation paths inside ATA helpers).
        SYSTEM_PROGRAM_ID,
        // Compute budget (set_compute_unit_limit / set_compute_unit_price
        // prepended automatically by RpcContext::build_signed).
        solana_sdk::compute_budget::ID,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist_has_five_programs() {
        let wl = whitelist_program_ids();
        assert_eq!(wl.len(), 5, "expected exactly 5 programs in hedgedjlp whitelist");
    }

    #[test]
    fn whitelist_includes_jupiter_perps() {
        assert!(whitelist_program_ids().contains(&JUPITER_PERPETUALS_PROGRAM_ID));
    }

    #[test]
    fn whitelist_includes_compute_budget() {
        // RpcContext::build_signed prepends compute budget ixns; without this
        // the verify_ixns guard would reject every transaction.
        assert!(whitelist_program_ids().contains(&solana_sdk::compute_budget::ID));
    }

    #[test]
    fn whitelist_excludes_kamino() {
        // Sanity check: hedgedjlp must NOT be allowed to sign Kamino lend
        // ixns. Pin this so a future copy-paste from stable-yield's whitelist
        // doesn't silently expand the daemon's mandate.
        use zerox1_defi_protocols::constants::KAMINO_LEND_PROGRAM_ID;
        let wl = whitelist_program_ids();
        assert!(!wl.contains(&KAMINO_LEND_PROGRAM_ID));
    }
}
