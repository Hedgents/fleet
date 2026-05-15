//! Hedgedjlp position reads.
//!
//! v0.2.6 (commit 1/2): live JLP balance pricing via Jupiter's lite Price
//! API. The hedge_positions Vec remains stubbed empty here; the live
//! Jupiter Perps Position decoder lands in commit 2/2.

use anyhow::Result;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use tracing::warn;
use zerox1_defi_protocols::constants::JLP_MINT;

use crate::chain::jlp_price;

#[derive(Debug, Clone, serde::Serialize)]
pub struct PositionView {
    /// JLP balance held by the operator wallet, lamports (6 decimals).
    pub jlp_balance_lamports: u64,
    /// Best-effort dollar value of the JLP held, micro-USD (1e-6 USD).
    /// Sourced from Jupiter's lite Price API; falls back to 0 on any
    /// transport / JSON error so the prior dashboard contract (silent
    /// zero) is preserved.
    pub jlp_value_usd_micro: u64,
    /// Open hedge legs. Live Jupiter Perps Position decoder lands in
    /// commit 2/2; for now this stays an empty Vec.
    pub hedge_positions: Vec<HedgePosition>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HedgePosition {
    pub asset: String,
    pub size_usd_micro: u64,
    pub side: String,
}

pub async fn read_jupiter_perps_position(rpc: &RpcClient, payer: &Pubkey) -> Result<PositionView> {
    let jlp_ata = get_associated_token_address(payer, &JLP_MINT);
    let jlp_balance_lamports = match rpc.get_token_account_balance(&jlp_ata).await {
        Ok(bal) => bal.amount.parse::<u64>().unwrap_or(0),
        Err(_) => 0,
    };

    let jlp_value_usd_micro = if jlp_balance_lamports == 0 {
        0
    } else {
        match jlp_price::fetch_jlp_price_micro_usd().await {
            Ok(price_micro) => jlp_price::value_micro_usd(jlp_balance_lamports, price_micro),
            Err(e) => {
                warn!(
                    ?e,
                    "jlp price fetch failed; reporting jlp_value_usd_micro=0"
                );
                0
            }
        }
    };

    Ok(PositionView {
        jlp_balance_lamports,
        jlp_value_usd_micro,
        hedge_positions: Vec::new(),
    })
}
