//! Hedgedjlp position reads.
//!
//! v0.2.6: live JLP balance pricing via Jupiter's lite Price API plus
//! on-chain discovery of any open Jupiter Perps short positions the
//! hedgedjlp daemon may have opened against SOL / BTC / ETH custodies.
//!
//! Earlier versions stubbed `jlp_value_usd_micro` to 0 and
//! `hedge_positions` to an empty Vec; the dashboard therefore reported a
//! deployed-USD of 0 even when the operator's wallet held priced JLP and
//! had active perp shorts. The two helpers below close that gap.

use anyhow::Result;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use tracing::{debug, warn};
use zerox1_defi_protocols::constants::{JLP_MINT, JLP_POOL, JUPITER_PERPETUALS_PROGRAM_ID};
use zerox1_defi_protocols::protocols::jlp::{decode_position, derive_position, PerpSide};

use crate::chain::jlp_price;

#[derive(Debug, Clone, serde::Serialize)]
pub struct PositionView {
    /// JLP balance held by the operator wallet, lamports (6 decimals).
    pub jlp_balance_lamports: u64,
    /// Best-effort dollar value of the JLP held, micro-USD (1e-6 USD).
    /// Sourced from Jupiter's lite Price API; falls back to 0 on any
    /// transport / JSON error so the prior behaviour (silent zero) is
    /// preserved.
    pub jlp_value_usd_micro: u64,
    /// Open Jupiter Perps short legs discovered for the operator wallet
    /// against the SOL / BTC / ETH custodies. Empty when no PDA exists
    /// (steady state before the daemon has opened any hedge) or when
    /// every discovered position has `size_usd == 0` (fully closed).
    pub hedge_positions: Vec<HedgePosition>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HedgePosition {
    /// Human-readable asset name — `"SOL"`, `"BTC"`, or `"ETH"`.
    pub asset: String,
    /// Position notional in USD (6 decimals).
    pub size_usd_micro: u64,
    /// Collateral remaining in USD (6 decimals).
    pub collateral_usd_micro: u64,
    /// `"Short"` always today (the hedgedjlp daemon never opens longs).
    pub side: String,
    /// Position PDA — frontend can deep-link to Solscan.
    pub position_pubkey: String,
}

/// SOL custody on the JLP pool (asset side for SOL shorts) — mainnet.
const SOL_CUSTODY_STR: &str = "7xS2gz2bTp3fwCC7knJvUWTEU9Tycczu6VhJYKgi1wdz";
/// BTC custody — mainnet.
const BTC_CUSTODY_STR: &str = "5Pv3gM9JrFFH883SWAhvJC9RPYmo8UNxuFtv5bMMALkm";
/// ETH custody — mainnet.
const ETH_CUSTODY_STR: &str = "AQCGyheWPLeo6Qp9WpYS9m3Qj479t7R636N9ey1rEjEn";
/// USDC custody — mainnet (collateral_custody for every hedgedjlp short).
const USDC_CUSTODY_STR: &str = "G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa";

fn watched_markets() -> [(&'static str, Pubkey, Pubkey, PerpSide); 3] {
    let usdc: Pubkey = USDC_CUSTODY_STR.parse().expect("USDC custody pubkey");
    [
        (
            "SOL",
            SOL_CUSTODY_STR.parse().expect("SOL custody"),
            usdc,
            PerpSide::Short,
        ),
        (
            "BTC",
            BTC_CUSTODY_STR.parse().expect("BTC custody"),
            usdc,
            PerpSide::Short,
        ),
        (
            "ETH",
            ETH_CUSTODY_STR.parse().expect("ETH custody"),
            usdc,
            PerpSide::Short,
        ),
    ]
}

pub async fn read_jupiter_perps_position(rpc: &RpcClient, payer: &Pubkey) -> Result<PositionView> {
    let jlp_ata = get_associated_token_address(payer, &JLP_MINT);
    let jlp_balance_lamports = match rpc.get_token_account_balance(&jlp_ata).await {
        Ok(bal) => bal.amount.parse::<u64>().unwrap_or(0),
        Err(_) => 0,
    };

    // Price the JLP held. Fall back to 0 on any API error — better than a
    // misleading approximation, and matches the prior dashboard contract.
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

    let hedge_positions = discover_hedge_positions(rpc, payer).await;

    Ok(PositionView {
        jlp_balance_lamports,
        jlp_value_usd_micro,
        hedge_positions,
    })
}

/// Probe the three (SOL/BTC/ETH, USDC, Short) Position PDAs for the
/// operator wallet on the Jupiter Perpetuals program. Returns one
/// [`HedgePosition`] per market where the PDA exists, is owned by the
/// perpetuals program, decodes cleanly, and carries a non-zero size.
///
/// One RPC call (`getMultipleAccounts`) covers all three markets. Any
/// RPC failure is logged at `warn!` and an empty Vec is returned —
/// keeps the dashboard responsive in degraded conditions.
async fn discover_hedge_positions(rpc: &RpcClient, owner: &Pubkey) -> Vec<HedgePosition> {
    let markets = watched_markets();
    let pool = JLP_POOL;
    let pdas: Vec<Pubkey> = markets
        .iter()
        .map(|(_asset, custody, coll, side)| derive_position(owner, &pool, custody, coll, *side))
        .collect();
    let accounts = match rpc.get_multiple_accounts(&pdas).await {
        Ok(a) => a,
        Err(e) => {
            warn!(
                ?e,
                "hedgedjlp position discovery: get_multiple_accounts failed"
            );
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for ((asset, _custody, _coll, _side), (pda, maybe_account)) in
        markets.iter().zip(pdas.iter().zip(accounts.into_iter()))
    {
        let Some(account) = maybe_account else {
            continue;
        };
        if account.owner != JUPITER_PERPETUALS_PROGRAM_ID {
            debug!(pda = %pda, owner = %account.owner, "PDA exists but wrong owner; skipping");
            continue;
        }
        match decode_position(*pda, &account.data) {
            Ok(pos) if !pos.is_empty() => {
                out.push(HedgePosition {
                    asset: (*asset).to_string(),
                    size_usd_micro: pos.size_usd,
                    collateral_usd_micro: pos.collateral_usd,
                    side: match pos.side {
                        PerpSide::Short => "Short".to_string(),
                        PerpSide::Long => "Long".to_string(),
                    },
                    position_pubkey: pda.to_string(),
                });
            }
            Ok(_) => debug!(pda = %pda, "Position PDA exists but is_empty(); skipping"),
            Err(e) => warn!(pda = %pda, ?e, "decode_position failed; skipping"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watched_markets_covers_three_shorts() {
        let m = watched_markets();
        assert_eq!(m.len(), 3);
        let names: Vec<&str> = m.iter().map(|(a, _, _, _)| *a).collect();
        assert_eq!(names, vec!["SOL", "BTC", "ETH"]);
        for (_, _, _, side) in m {
            assert_eq!(side, PerpSide::Short, "hedgedjlp only opens shorts");
        }
    }

    #[test]
    fn custody_constants_parse() {
        let _: Pubkey = SOL_CUSTODY_STR.parse().expect("SOL custody pubkey");
        let _: Pubkey = BTC_CUSTODY_STR.parse().expect("BTC custody pubkey");
        let _: Pubkey = ETH_CUSTODY_STR.parse().expect("ETH custody pubkey");
        let _: Pubkey = USDC_CUSTODY_STR.parse().expect("USDC custody pubkey");
    }
}
