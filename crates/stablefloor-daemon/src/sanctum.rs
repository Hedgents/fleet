//! Sanctum INF integration — placeholder stub.
//!
//! The original handler from the monolith is not yet committed (uncommitted
//! in parallel checkout). Strategy follow-up plan: replace these stubs with
//! real Sanctum mint/redeem ixn construction + RPC submission.

use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use tracing::info;
use zerox1_defi_wallet::{SigningWhitelist, Wallet};

/// Program IDs the StableFloor daemon is allowed to sign for. Anything else
/// is rejected by the wallet whitelist before signing.
pub fn program_ids() -> Vec<Pubkey> {
    vec![
        // Sanctum INF (placeholder — strategy plan must verify against mainnet).
        Pubkey::from_str("5ocnV1qiCgedGjVDL4pT4SfbCyD3WjZjVHDQjvSv6cYn").unwrap(),
    ]
}

/// Mint Sanctum INF from `sol_amount` SOL. Strategy plan implements the real path.
pub async fn mint(_wallet: &Wallet, _whitelist: &SigningWhitelist, sol_amount: f64) -> Result<()> {
    info!(
        sol_amount,
        "stablefloor::mint (stub — strategy plan implements)"
    );
    Ok(())
}

/// Redeem `inf_amount` Sanctum INF back to SOL. Strategy plan implements the real path.
pub async fn redeem(
    _wallet: &Wallet,
    _whitelist: &SigningWhitelist,
    inf_amount: f64,
) -> Result<()> {
    info!(
        inf_amount,
        "stablefloor::redeem (stub — strategy plan implements)"
    );
    Ok(())
}
