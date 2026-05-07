//! Hedgedjlp position reads.
//!
//! v0 scope: read the operator's JLP token balance from their JLP ATA.
//! Hedge positions remain stubbed (empty Vec) until the live Jupiter
//! Perpetuals custody loader lands — see post-demo work in the hedgedjlp
//! crate. This is sufficient for the dashboard to display "you hold N
//! JLP" without surfacing perp shorts; live hedge totals come from the
//! daemon's pnl JSONL telemetry already ingested into `pnl_snapshots`.

use anyhow::Result;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use zerox1_defi_protocols::constants::JLP_MINT;

#[derive(Debug, Clone, serde::Serialize)]
pub struct PositionView {
    /// JLP balance held by the operator wallet, lamports (6 decimals).
    pub jlp_balance_lamports: u64,
    /// Best-effort dollar value of the JLP held, micro-USD (1e-6 USD).
    /// v0 stubs to 0 until JLP NAV oracle read lands.
    pub jlp_value_usd_micro: u64,
    /// Open hedge legs. Empty until live Jupiter Perps custody loader
    /// lands; daemon-side pnl JSONL is the authoritative source today.
    pub hedge_positions: Vec<HedgePosition>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HedgePosition {
    pub asset: String,
    pub size_usd_micro: u64,
    pub side: String,
}

pub async fn read_jupiter_perps_position(
    rpc: &RpcClient,
    payer: &Pubkey,
) -> Result<PositionView> {
    let jlp_ata = get_associated_token_address(payer, &JLP_MINT);
    let jlp_balance_lamports = match rpc.get_token_account_balance(&jlp_ata).await {
        Ok(bal) => bal.amount.parse::<u64>().unwrap_or(0),
        Err(_) => 0,
    };
    Ok(PositionView {
        jlp_balance_lamports,
        // v0: NAV oracle integration is a post-demo follow-up. Leaving 0
        // is safer than surfacing a misleading approximation.
        jlp_value_usd_micro: 0,
        hedge_positions: Vec::new(),
    })
}
