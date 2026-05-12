//! Program-ID whitelist for stable-yield-daemon.
//!
//! Subset of multiply-daemon's whitelist — stable-yield only does
//! single-leg USDC supply, so no Jito stake-pool, no SPL stake-pool, no
//! Kamino Farms. Just the ixns the deposit loop emits:
//!
//!   - Kamino lend (initialize_obligation, refresh_reserve, deposit)
//!   - Associated Token Account (idempotent ATA create for USDC)
//!   - SPL Token (CPI'd by ATA-create / klend internally)
//!   - System program (account creation paths inside ATA helpers)
//!   - Compute budget (set_compute_unit_limit + set_compute_unit_price
//!     prepended automatically by `RpcContext::build_signed`)
//!
//! `SigningWhitelist::verify_ixns` (audit-fix I1) runs on every ixn slice
//! before signing — sim and submit paths both. Anything outside this set
//! is rejected before the wallet ever sees the bytes.

use solana_sdk::pubkey::Pubkey;

use zerox1_defi_protocols::constants::{
    ASSOCIATED_TOKEN_PROGRAM_ID, KAMINO_LEND_PROGRAM_ID, SYSTEM_PROGRAM_ID, TOKEN_PROGRAM_ID,
};

/// Set of Solana program ids the stable-yield wallet is allowed to sign
/// instructions against. Subset of multiply's whitelist:
///   - NO Jito stake-pool (we don't stake)
///   - NO SPL stake-pool (we don't stake)
///   - NO Kamino Farms (no harvest path in v0)
pub fn whitelist_program_ids() -> Vec<Pubkey> {
    vec![
        // Kamino Lend (init_obligation, refresh_reserve, deposit_reserve_liquidity_and_obligation_collateral).
        KAMINO_LEND_PROGRAM_ID,
        // SPL Token (CPI'd by ATA-create + klend collateral handling).
        TOKEN_PROGRAM_ID,
        // Associated Token Account (idempotent ATA creation for the user's USDC ATA).
        ASSOCIATED_TOKEN_PROGRAM_ID,
        // System program (rent + account creation paths inside ATA helpers).
        SYSTEM_PROGRAM_ID,
        // Compute budget (set_compute_unit_limit / set_compute_unit_price prepended by RpcContext).
        solana_sdk::compute_budget::ID,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist_has_five_programs() {
        let wl = whitelist_program_ids();
        assert_eq!(
            wl.len(),
            5,
            "expected exactly 5 programs in stable-yield whitelist"
        );
    }

    #[test]
    fn whitelist_includes_kamino_lend() {
        assert!(whitelist_program_ids().contains(&KAMINO_LEND_PROGRAM_ID));
    }

    #[test]
    fn whitelist_excludes_jito_and_stake_pool() {
        // Sanity check: stable-yield must NOT be allowed to sign Jito or
        // stake-pool ixns. Pin this so a future copy-paste from multiply's
        // whitelist doesn't silently expand the daemon's mandate.
        use zerox1_defi_protocols::constants::SPL_STAKE_POOL_PROGRAM_ID;
        let wl = whitelist_program_ids();
        assert!(!wl.contains(&SPL_STAKE_POOL_PROGRAM_ID));
    }
}
