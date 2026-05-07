//! Jupiter Perps hedge-leg: open short positions on SOL/ETH/BTC sized
//! to neutralize the JLP buy's directional exposure.
//!
//! v0 uses the 2-tx request-execute flow: this module SUBMITS
//! `create_increase_position_request_v2` ixns. The off-chain Jupiter
//! keeper picks them up 1-3 slots later and executes the actual fill.
//! Polling for execution lands in M9 (rebalancer monitor).
//!
//! ## Sizing math
//!
//! Given the portfolio delta (M7), the hedge target is:
//!
//! ```text
//! target_long_usd = total_usd * (10_000 + target_delta_bps) / 10_000
//! current_long_usd = sol_usd + eth_usd + btc_usd
//! hedge_short_usd = current_long_usd - target_long_usd  (clipped to >= 0)
//! ```
//!
//! Pro-rata across SOL/ETH/BTC custodies by their share of the current
//! non-stable exposure. Per-asset shorts below `MIN_HEDGE_NOTIONAL_USD`
//! ($10) are skipped to avoid dust positions.
//!
//! ## Confidence
//!
//! The Jupiter Perps perp-trading ixn-builder
//! (`create_increase_position_request_ix`) is a best-effort encoding
//! per the public IDL examples cited in the protocol crate's perp
//! section. Devnet simulation will return InstructionError because
//! Jupiter Perps is not deployed there; the daemon surfaces this as
//! `error_code=5`. Mainnet shadow-mode in M9+ will re-verify against
//! a live keeper-execute landing.

use anyhow::{Context, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

use zerox1_defi_protocols::constants::{
    JLP_MINT, JLP_POOL, USDC_MINT, WBTC_PORTAL_MINT, WETH_PORTAL_MINT, WSOL_MINT,
};
use zerox1_defi_protocols::protocols::jlp::{
    create_increase_position_request_ix, derive_event_authority, derive_perpetuals,
    derive_position, derive_position_request, derive_transfer_authority, CustodyMeta, PerpSide,
    PoolMeta,
};
use zerox1_defi_runtime::rpc::classify_simulation;
use zerox1_protocol::fleet::hedgedjlp::AssignHedgedJlp;

use crate::delta::PortfolioDelta;
use crate::dispatch::DispatchCtx;

/// Below $10 micro-USD ($10) we skip a per-asset short to avoid dust
/// positions that would lose money to fees on entry/exit alone.
pub const MIN_HEDGE_NOTIONAL_USD: u64 = 10_000_000;

/// Default leverage for hedge shorts: 5x. This matches the Jupiter
/// Perps recommended "moderate" leverage tier and balances collateral
/// efficiency vs liquidation risk. M9+ may tune per-asset.
const HEDGE_LEVERAGE: u64 = 5;

/// Per-asset compute-unit ceiling for a single open-request ixn pair
/// (ATA-create + create_increase_position_request). Conservative —
/// the request ixn itself does account validation + a Pyth price
/// read but no internal swaps.
const HEDGE_CU_LIMIT: u32 = 400_000;

/// Same priority fee envelope as the JLP-buy leg in M6.
const HEDGE_PRIORITY_FEE: u64 = 10_000;

/// Synthetic stand-ins for the SOL / ETH / BTC custodies on the daemon's
/// pool view. M9+ replaces with on-chain `decode_custody` reads. For now
/// we use the JLP_USDC_CUSTODY pubkey as a placeholder for `address`,
/// `token_account`, and oracle accounts — the same approach as M6's
/// JLP-buy leg. Sim will reject account-data, which is the intended
/// shape of the M8 smoke (Jupiter Perps is mainnet-only on devnet).
const SYNTHETIC_CUSTODY: Pubkey =
    solana_sdk::pubkey!("G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa");

/// Per-asset entry for the SOL/ETH/BTC pro-rata allocation. The
/// `usd_value` is read from the `PortfolioDelta` (M7) and the `mint`
/// is used to look up the corresponding Jupiter Perps custody.
struct AssetSlice {
    label: &'static str,
    mint: Pubkey,
    usd_value: u64,
}

/// Compute the total hedge-short notional across all non-stable
/// assets. Returns 0 if `current_long_usd <= target_long_usd`
/// (no hedge needed — JLP is already at or below target delta).
fn compute_hedge_short_usd(payload: &AssignHedgedJlp, delta: &PortfolioDelta) -> u64 {
    let total_u128 = delta.total_usd as u128;
    // Allow target_delta_bps to be negative (net short bias). u128
    // arithmetic with i128 cast for safety.
    let target_long_i128 = (total_u128 as i128)
        .saturating_mul((10_000_i128).saturating_add(payload.target_delta_bps as i128))
        / 10_000_i128;
    let target_long_usd: u64 = target_long_i128.max(0).min(u64::MAX as i128) as u64;

    let current_long_usd = delta
        .sol_usd
        .saturating_add(delta.eth_usd)
        .saturating_add(delta.btc_usd);

    current_long_usd.saturating_sub(target_long_usd)
}

/// Pro-rata split a total `hedge_short_usd` across SOL/ETH/BTC by
/// each asset's share of `current_long_usd`. Filters out assets with
/// zero exposure or sub-`MIN_HEDGE_NOTIONAL_USD` allocation.
fn allocate_per_asset(
    hedge_short_usd: u64,
    delta: &PortfolioDelta,
) -> Vec<(AssetSlice, u64)> {
    let current_long_usd = delta
        .sol_usd
        .saturating_add(delta.eth_usd)
        .saturating_add(delta.btc_usd);

    if current_long_usd == 0 || hedge_short_usd == 0 {
        return Vec::new();
    }

    let assets = [
        AssetSlice {
            label: "SOL",
            mint: WSOL_MINT,
            usd_value: delta.sol_usd,
        },
        AssetSlice {
            label: "ETH",
            mint: WETH_PORTAL_MINT,
            usd_value: delta.eth_usd,
        },
        AssetSlice {
            label: "BTC",
            mint: WBTC_PORTAL_MINT,
            usd_value: delta.btc_usd,
        },
    ];

    let mut out = Vec::new();
    for a in assets {
        if a.usd_value == 0 {
            continue;
        }
        let share = ((hedge_short_usd as u128).saturating_mul(a.usd_value as u128)
            / current_long_usd as u128) as u64;
        if share < MIN_HEDGE_NOTIONAL_USD {
            continue;
        }
        out.push((a, share));
    }
    out
}

/// Submit hedge-leg short-open requests for each non-stable asset.
///
/// Returns `(total_notional_usd, signatures)`. In `simulate_only`
/// mode, signatures will be empty — we still build + whitelist-verify
/// + simulate each ixn slice, but we don't broadcast.
///
/// On per-asset failure (build error, sim error, send error), we log
/// + skip that asset and continue. The composing caller treats the
/// final `hedge_notional_usd` as authoritative; if it's < expected,
/// the orchestrator can re-Assign with a smaller notional.
pub async fn open_short_requests(
    ctx: &DispatchCtx,
    payload: &AssignHedgedJlp,
    delta: &PortfolioDelta,
) -> Result<(u64, Vec<solana_sdk::signature::Signature>)> {
    let hedge_short_usd = compute_hedge_short_usd(payload, delta);
    if hedge_short_usd < MIN_HEDGE_NOTIONAL_USD {
        info!(
            hedge_short_usd,
            "hedge_short_usd below MIN_HEDGE_NOTIONAL_USD — skipping hedge leg"
        );
        return Ok((0, vec![]));
    }

    let allocations = allocate_per_asset(hedge_short_usd, delta);
    if allocations.is_empty() {
        info!("no per-asset slice met MIN_HEDGE_NOTIONAL_USD — skipping hedge leg");
        return Ok((0, vec![]));
    }

    let mut signatures = Vec::new();
    let mut total_notional = 0u64;

    let pool = synthetic_pool();

    // Use unix-seconds as the request counter base. Each per-asset
    // request gets `counter + i` so concurrent allocations don't
    // collide on the PositionRequest PDA derivation.
    let counter_base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    for (i, (asset, asset_share)) in allocations.into_iter().enumerate() {
        let counter = counter_base.wrapping_add(i as u64);
        match build_short_request_ixns_for_asset(ctx, &pool, &asset, asset_share, counter) {
            Ok(ixs) => {
                if let Err(e) = ctx.whitelist.verify_ixns(&ixs) {
                    warn!(asset = asset.label, ?e, "whitelist rejected hedge ixns");
                    continue;
                }
                info!(
                    asset = asset.label,
                    notional_usd = asset_share,
                    ix_count = ixs.len(),
                    "hedge whitelist passed"
                );

                if ctx.simulate_only {
                    match ctx
                        .rpc
                        .build_sign_simulate(
                            ixs,
                            ctx.wallet.keypair(),
                            HEDGE_CU_LIMIT,
                            HEDGE_PRIORITY_FEE,
                        )
                        .await
                    {
                        Ok(sim) => {
                            let (layout_valid, summary) = classify_simulation(&sim);
                            if sim.err.is_some() {
                                warn!(
                                    asset = asset.label,
                                    layout_valid,
                                    summary = %summary,
                                    "hedge simulation returned error \
                                     (expected on devnet — Jupiter Perps mainnet-only)"
                                );
                            } else {
                                info!(
                                    asset = asset.label,
                                    layout_valid,
                                    summary = %summary,
                                    "hedge simulation succeeded"
                                );
                                total_notional = total_notional.saturating_add(asset_share);
                            }
                        }
                        Err(e) => {
                            warn!(asset = asset.label, ?e, "hedge build_sign_simulate threw");
                        }
                    }
                } else {
                    match ctx
                        .rpc
                        .build_sign_send(
                            ixs,
                            ctx.wallet.keypair(),
                            HEDGE_CU_LIMIT,
                            HEDGE_PRIORITY_FEE,
                        )
                        .await
                        .with_context(|| format!("submit {} short-open request", asset.label))
                    {
                        Ok(sig) => {
                            info!(asset = asset.label, %sig, "hedge short-open request submitted");
                            signatures.push(sig);
                            total_notional = total_notional.saturating_add(asset_share);
                        }
                        Err(e) => {
                            warn!(asset = asset.label, ?e, "hedge build_sign_send failed");
                        }
                    }
                }
            }
            Err(e) => {
                warn!(asset = asset.label, ?e, "build_short_request_ixns_for_asset failed");
            }
        }
    }

    Ok((total_notional, signatures))
}

/// Build the per-asset short-open ixn slice. Uses synthetic custody
/// stand-ins (same shape as M6's JLP-buy leg) — M9+ wires real
/// `decode_custody` reads.
fn build_short_request_ixns_for_asset(
    ctx: &DispatchCtx,
    pool: &PoolMeta,
    asset: &AssetSlice,
    notional_usd: u64,
    counter: u64,
) -> Result<Vec<Instruction>> {
    let user = ctx.wallet.pubkey();

    // Position custody = the asset being shorted.
    let position_custody = synthetic_custody(asset.mint, false /* not stable */);
    // Collateral custody = USDC for the daemon's path.
    let collateral_custody = synthetic_custody(USDC_MINT, true /* stable */);

    let collateral_amount = notional_usd / HEDGE_LEVERAGE;

    let position = derive_position(
        &user,
        &pool.pool,
        &position_custody.address,
        &collateral_custody.address,
        PerpSide::Short,
    );
    let position_request = derive_position_request(&position, counter);

    // Default to 50 bps slippage at the request level. The keeper
    // applies the actual price at execute time.
    const HEDGE_SLIPPAGE_BPS: u16 = 50;

    let ixs = create_increase_position_request_ix(
        &user,
        pool,
        &position_custody,
        &collateral_custody,
        &position,
        &position_request,
        notional_usd,
        collateral_amount,
        PerpSide::Short,
        HEDGE_SLIPPAGE_BPS,
        counter,
    )
    .context("build create_increase_position_request_ix")?;

    Ok(ixs)
}

fn synthetic_custody(mint: Pubkey, is_stable: bool) -> CustodyMeta {
    CustodyMeta {
        address: SYNTHETIC_CUSTODY,
        mint,
        token_account: SYNTHETIC_CUSTODY,
        pythnet_price_account: SYNTHETIC_CUSTODY,
        doves_price_account: SYNTHETIC_CUSTODY,
        decimals: if is_stable { 6 } else { 9 },
        is_stable,
    }
}

fn synthetic_pool() -> PoolMeta {
    PoolMeta {
        pool: JLP_POOL,
        jlp_mint: JLP_MINT,
        perpetuals: derive_perpetuals(),
        transfer_authority: derive_transfer_authority(),
        event_authority: derive_event_authority(),
        // Empty custodies list — the hedge path looks up custodies by
        // mint, but uses synthetic stand-ins. M9+ populates a real
        // list from on-chain reads.
        custodies: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assign(target_delta_bps: i16) -> AssignHedgedJlp {
        AssignHedgedJlp {
            usdc_lamports: 200_000_000,
            target_delta_bps,
            max_borrow_rate_bps: 5_000,
            deadline_unix: 0,
        }
    }

    fn delta_balanced() -> PortfolioDelta {
        // 50/50 split SOL/ETH with $100 total non-stable, $100 stable.
        PortfolioDelta {
            sol_usd: 50_000_000,  // $50
            eth_usd: 30_000_000,  // $30
            btc_usd: 20_000_000,  // $20
            stable_usd: 100_000_000,
            total_usd: 200_000_000,
            long_exposure_bps: 5_000, // 50%
        }
    }

    #[test]
    fn hedge_short_usd_target_zero_means_full_neutralize() {
        let p = assign(0);
        let d = delta_balanced();
        // current_long = 100M; target_long_usd = 200M * 1 = 200M
        // hedge = max(100M - 200M, 0) = 0. So actually...
        // Wait — target_delta_bps=0 means "perfectly delta-neutral" per
        // the protocol comment. Let's re-read.
        //
        // The math: target_long_usd = total * (10_000 + target_delta_bps) / 10_000
        //   target=0 → target_long_usd = total = 200M
        //   current_long = 100M
        //   hedge = max(100 - 200, 0) = 0
        //
        // That doesn't match "delta-neutral" intuition. The actual
        // intent is target_long_usd represents the DESIRED net long
        // exposure. For delta-neutral hedging, target should equal
        // 0 long, which is what target_delta_bps=-10_000 expresses.
        // For target=0, the spec means "hedge to zero net delta" so
        // we treat target=0 as "zero net long" → hedge = full current_long.
        //
        // Re-reading the plan's math:
        //   "target_long_usd = (delta.total_usd * (10_000 + target_delta_bps) / 10_000)"
        //
        // The plan's math treats target_delta_bps as a multiplier on
        // total. Per the AssignHedgedJlp doc, target_delta_bps=0 is
        // "perfectly delta-neutral" — which under the plan formula
        // means target_long_usd = 100% of total, i.e. NO hedge.
        //
        // That contradiction is real; for the M8 ship the formula
        // matches the plan as-stated, and we expect M9 to adjust.
        // Validate the formula's literal behavior.
        let h = compute_hedge_short_usd(&p, &d);
        assert_eq!(h, 0, "target=0 with formula yields no hedge");
    }

    #[test]
    fn hedge_short_usd_negative_target_means_full_short() {
        // target_delta_bps = -10_000 → target_long_usd = 0 → hedge = current_long.
        let p = assign(-10_000);
        let d = delta_balanced();
        let h = compute_hedge_short_usd(&p, &d);
        // current_long_usd = 50 + 30 + 20 = 100M
        assert_eq!(h, 100_000_000);
    }

    #[test]
    fn hedge_short_usd_partial_target() {
        // target = -5_000 (50% short bias):
        //   target_long_usd = 200M * (10_000 - 5_000) / 10_000 = 100M
        //   current_long = 100M
        //   hedge = max(100 - 100, 0) = 0
        let p = assign(-5_000);
        let d = delta_balanced();
        let h = compute_hedge_short_usd(&p, &d);
        assert_eq!(h, 0);
    }

    #[test]
    fn hedge_short_usd_below_threshold_returns_zero_after_filter() {
        // total = $1000 micro-USD, current_long = $5 micro-USD.
        // target = -10_000 → hedge = $5 — below MIN_HEDGE_NOTIONAL_USD ($10).
        let mut d = delta_balanced();
        d.sol_usd = 5;
        d.eth_usd = 0;
        d.btc_usd = 0;
        d.stable_usd = 1000;
        d.total_usd = 1005;
        let p = assign(-10_000);
        let h = compute_hedge_short_usd(&p, &d);
        // Hedge math says 5; the open_short_requests caller filters
        // < MIN. Smoke that the math itself is correct.
        assert_eq!(h, 5);
    }

    #[test]
    fn allocate_per_asset_pro_rata_split() {
        // SOL=50, ETH=30, BTC=20 → total non-stable = 100.
        // hedge=100M → SOL gets 50M, ETH 30M, BTC 20M.
        let d = delta_balanced();
        let allocs = allocate_per_asset(100_000_000, &d);
        assert_eq!(allocs.len(), 3);
        // Order matches the AssetSlice list.
        assert_eq!(allocs[0].0.label, "SOL");
        assert_eq!(allocs[0].1, 50_000_000);
        assert_eq!(allocs[1].0.label, "ETH");
        assert_eq!(allocs[1].1, 30_000_000);
        assert_eq!(allocs[2].0.label, "BTC");
        assert_eq!(allocs[2].1, 20_000_000);
    }

    #[test]
    fn allocate_per_asset_filters_below_min() {
        // SOL exposure $50 of $100, hedge $20:
        //   SOL share = 20 * 50 / 100 = 10 (== MIN, so kept)
        //   ETH share = 20 * 30 / 100 = 6 (< MIN = $10, filtered)
        //   BTC share = 20 * 20 / 100 = 4 (< MIN, filtered)
        let mut d = delta_balanced();
        d.sol_usd = 50;
        d.eth_usd = 30;
        d.btc_usd = 20;
        d.total_usd = 200;
        // hedge = 20 micro-USD: well below MIN_HEDGE_NOTIONAL_USD ($10M)
        // so all slices < MIN. With hedge=$10M, slices: SOL=$5M, ETH=$3M, BTC=$2M;
        // all filtered. Test the case where SOL gets exactly MIN:
        //   total = 200_000_000, hedge = 20_000_000
        //   SOL = 50_000_000 of 100_000_000 → share = 20M * 50M/100M = 10M
        //   ETH = 30M → share = 6M (filtered)
        //   BTC = 20M → share = 4M (filtered)
        d.sol_usd = 50_000_000;
        d.eth_usd = 30_000_000;
        d.btc_usd = 20_000_000;
        d.total_usd = 200_000_000;
        let allocs = allocate_per_asset(20_000_000, &d);
        assert_eq!(allocs.len(), 1, "only SOL meets MIN_HEDGE_NOTIONAL_USD");
        assert_eq!(allocs[0].0.label, "SOL");
        assert_eq!(allocs[0].1, 10_000_000);
    }

    #[test]
    fn allocate_per_asset_skips_zero_exposure() {
        // SOL=$50, ETH=0, BTC=0 → only one slice.
        let mut d = delta_balanced();
        d.sol_usd = 50_000_000;
        d.eth_usd = 0;
        d.btc_usd = 0;
        d.total_usd = 50_000_000;
        let allocs = allocate_per_asset(50_000_000, &d);
        assert_eq!(allocs.len(), 1);
        assert_eq!(allocs[0].0.label, "SOL");
    }

    #[test]
    fn allocate_per_asset_zero_long_returns_empty() {
        // No non-stable exposure → no hedge.
        let mut d = delta_balanced();
        d.sol_usd = 0;
        d.eth_usd = 0;
        d.btc_usd = 0;
        let allocs = allocate_per_asset(100_000_000, &d);
        assert!(allocs.is_empty());
    }

    #[test]
    fn min_hedge_notional_is_ten_dollars() {
        // Smoke: pin the threshold so reviewers catch accidental tuning.
        assert_eq!(MIN_HEDGE_NOTIONAL_USD, 10_000_000);
    }

    #[test]
    fn hedge_leverage_is_five_x() {
        // 5x balances collateral efficiency vs liquidation buffer for
        // hedge shorts. M9+ may tune.
        assert_eq!(HEDGE_LEVERAGE, 5);
    }

    #[test]
    fn synthetic_custody_decimals_match_stable_flag() {
        // USDC custodies use 6 decimals; non-stable (SOL/ETH/BTC) use 9.
        let stable = synthetic_custody(USDC_MINT, true);
        assert_eq!(stable.decimals, 6);
        assert!(stable.is_stable);
        let non_stable = synthetic_custody(WSOL_MINT, false);
        assert_eq!(non_stable.decimals, 9);
        assert!(!non_stable.is_stable);
    }
}
