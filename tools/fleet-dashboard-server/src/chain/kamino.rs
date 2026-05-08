//! Kamino obligation reads for the dashboard.
//!
//! Wraps `zerox1_defi_protocols::protocols::kamino_loader::fetch_obligation`
//! into two best-effort views:
//! - `ObligationView` for the multiply daemon (deposited + borrowed +
//!   computed LTV in bps).
//! - `SupplyView` for the stable-yield daemon (deposited cTokens treated
//!   as the supply position; for the dashboard's purposes we surface the
//!   raw deposited amount and the reserve pubkey).
//!
//! Both readers return `Ok(None)` if the obligation account doesn't exist
//! yet (fresh wallet) — this keeps the dashboard responsive while the
//! operator is still funding.

use anyhow::Result;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use zerox1_defi_protocols::protocols::kamino;
use zerox1_defi_protocols::protocols::kamino_loader;

/// Multiply's obligation view: deposited collateral, debt, current LTV.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ObligationView {
    pub obligation_pubkey: Pubkey,
    pub ltv_bps: u16,
    /// Deposited collateral USD value, micro-units (1e-6 USD).
    pub deposited_usd_micro: u64,
    /// Borrowed assets USD value, micro-units.
    pub borrowed_usd_micro: u64,
}

/// Stable-yield's supply view: deposited USDC into a Kamino reserve.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SupplyView {
    pub reserve_pubkey: Pubkey,
    /// Deposited USDC in lamports (1e-6 USDC, 6 decimals).
    pub deposited_usdc_lamports: u64,
}

/// Read multiply's obligation. `payer` is the operator wallet, `market` is
/// the Kamino main lending market PDA.
pub async fn read_multiply_obligation(
    rpc: &RpcClient,
    payer: &Pubkey,
    market: &Pubkey,
) -> Result<Option<ObligationView>> {
    let obligation_pk = kamino::derive_user_obligation(payer, market);
    let Some(decoded) = kamino_loader::fetch_obligation(rpc, &obligation_pk).await? else {
        return Ok(None);
    };
    if decoded.deposits.is_empty() && decoded.borrows.is_empty() {
        return Ok(None);
    }
    let ltv_bps = if decoded.deposited_value_sf == 0 {
        0u16
    } else {
        let ratio = decoded
            .borrowed_assets_market_value_sf
            .saturating_mul(10_000)
            .checked_div(decoded.deposited_value_sf)
            .unwrap_or(0);
        ratio.min(u16::MAX as u128) as u16
    };
    // sf-scaled USD: divide by 2^60 to get USD; we render in micro-USD
    // (1e-6 USD) so multiply by 1e6 then shift by 60.
    let deposited_usd_micro = sf_to_micro_usd(decoded.deposited_value_sf);
    let borrowed_usd_micro = sf_to_micro_usd(decoded.borrowed_assets_market_value_sf);
    Ok(Some(ObligationView {
        obligation_pubkey: obligation_pk,
        ltv_bps,
        deposited_usd_micro,
        borrowed_usd_micro,
    }))
}

/// Read stable-yield's supply view. We surface the deposited USDC (in
/// lamports) found on the obligation against the named reserve.
///
/// Returns `Ok(None)` if no obligation exists or no deposit against
/// `reserve` is present.
pub async fn read_stable_yield_supply(
    rpc: &RpcClient,
    payer: &Pubkey,
    market: &Pubkey,
    reserve: &Pubkey,
) -> Result<Option<SupplyView>> {
    let obligation_pk = kamino::derive_user_obligation(payer, market);
    let Some(decoded) = kamino_loader::fetch_obligation(rpc, &obligation_pk).await? else {
        return Ok(None);
    };
    let Some(deposit) = decoded.deposits.iter().find(|d| &d.reserve == reserve) else {
        return Ok(None);
    };
    Ok(Some(SupplyView {
        reserve_pubkey: *reserve,
        // `deposited_amount` is cToken units; for dashboard purposes we
        // surface it as-is. Conversion to underlying USDC requires the
        // reserve's exchange rate which the dashboard doesn't track yet.
        deposited_usdc_lamports: deposit.deposited_amount,
    }))
}

fn sf_to_micro_usd(sf: u128) -> u64 {
    // sf is value * 2^60. We want value * 1_000_000.
    // value_micro = sf * 1_000_000 / 2^60 = sf >> 60 * 1_000_000 (lossy
    // for high precision but fine for dashboard display).
    let usd = (sf >> 60) as u128;
    let micro = usd.saturating_mul(1_000_000);
    micro.min(u64::MAX as u128) as u64
}
