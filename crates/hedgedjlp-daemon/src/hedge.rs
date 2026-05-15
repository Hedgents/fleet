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
//! Given the portfolio delta (M7), the hedge target is computed by
//! interpreting `target_delta_bps` as the desired NET exposure ratio of
//! `total_usd`. Allowed range is `[-10_000, +10_000]` (clamped by caps).
//!
//! ```text
//! target_net_long_usd_signed = total_usd * target_delta_bps / 10_000      (signed)
//! target_net_long_usd        = max(target_net_long_usd_signed, 0)
//! target_net_short_usd       = max(-target_net_long_usd_signed, 0)
//! current_long_usd           = sol_usd + eth_usd + btc_usd
//! hedge_short_usd            = current_long_usd
//!                                 - target_net_long_usd
//!                                 + target_net_short_usd                  (saturating)
//! ```
//!
//! Verification cases (total=$1000 micro-USD, current_long=$1000):
//!   target_delta_bps =      0 → target_net=0   → hedge=$1000 (full neutralization)
//!   target_delta_bps =   +500 → target_net=+50 → hedge=$950  (small long bias)
//!   target_delta_bps =   -500 → target_net=-50 → hedge=$1050 (small short bias)
//!   target_delta_bps = +10000 → target_net=+1000 → hedge=$0   (pure long)
//!   target_delta_bps = -10000 → target_net=-1000 → hedge=$2000 (max short bias)
//!
//! M8 historical note: M8 shipped a broken formula that treated
//! `target_delta_bps=0` as "no hedge" (target_long_usd = 100% of total).
//! M9 corrects the interpretation per the docstring above.
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
    PoolMeta, RequestChange,
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
/// (ATA-create + create_increase_position_market_request). Bumped to
/// 600k to match the Jupiter SDK example envelope for a 16-account
/// request ix — audit fix (cosmetic) for spec §3.
const HEDGE_CU_LIMIT: u32 = 600_000;

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

/// Hard-coded "sensible" mark prices in 6-decimal USD scale used to
/// compute `price_slippage` for sim-only runs. Audit fix 8 / spec §3
/// requires this be a 6-decimal USD price, NOT bps. Sim-only is
/// fine with a stale-but-reasonable number — production must replace
/// with a live Pyth/Doves read before any broadcast (which we are
/// NOT doing per the v0.2.1 mandate).
///
/// For a Short open, slippage price = mark * (1 + buffer). 1% buffer.
pub(crate) fn sim_mark_price_micro_usd(label: &str) -> u64 {
    match label {
        // 1 SOL @ $150 = 150_000_000 micro-USD
        "SOL" => 150_000_000,
        // 1 ETH @ $3500
        "ETH" => 3_500_000_000,
        // 1 BTC @ $70_000
        "BTC" => 70_000_000_000,
        _ => 100_000_000,
    }
}

/// Compute the total hedge-short notional across all non-stable
/// assets, interpreting `target_delta_bps` as the desired NET exposure
/// ratio of `total_usd`.
///
/// Returns 0 if the current long exposure is already <= desired net long
/// (and target is non-negative). When target is negative (net-short
/// bias), `hedge_short_usd` exceeds `current_long_usd` so the protocol
/// also opens uncollateralized shorts.
pub(crate) fn compute_hedge_short_usd(payload: &AssignHedgedJlp, delta: &PortfolioDelta) -> u64 {
    let total_i128 = delta.total_usd as i128;
    let bps = payload.target_delta_bps as i128;

    // Signed net target. Cap-validation already restricts bps to
    // [-10_000, +10_000] via caps::validate_assign, but use saturating
    // math anyway in case a future cap relaxes the bound.
    let target_net_long_usd_signed = total_i128.saturating_mul(bps) / 10_000;
    let target_net_long_usd: u64 = target_net_long_usd_signed.max(0).min(u64::MAX as i128) as u64;
    let target_net_short_usd: u64 =
        (-target_net_long_usd_signed).max(0).min(u64::MAX as i128) as u64;

    let current_long_usd = delta
        .sol_usd
        .saturating_add(delta.eth_usd)
        .saturating_add(delta.btc_usd);

    current_long_usd
        .saturating_sub(target_net_long_usd)
        .saturating_add(target_net_short_usd)
}

/// Pro-rata split a total `hedge_short_usd` across SOL/ETH/BTC by
/// each asset's share of `current_long_usd`. Filters out assets with
/// zero exposure or sub-`MIN_HEDGE_NOTIONAL_USD` allocation.
fn allocate_per_asset(hedge_short_usd: u64, delta: &PortfolioDelta) -> Vec<(AssetSlice, u64)> {
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

/// Composite return from `open_short_requests`. Audit-fix C1/C2/I5:
///
/// - `total_notional` is credited only on submit success (audit-fix
///   I5 — sim-only no longer inflates the Report).
/// - `signatures` collects on-chain submit signatures (empty in
///   sim-only mode).
/// - `open_positions` records `(asset_label, position_pubkey,
///   open_counter)` for each successfully-submitted open. The unwind
///   path consumes the counter to derive the matching close-request
///   PDA (audit-fix C2 — close PDAs match open PDAs).
/// - `sim_only` flags a sim run so the caller knows NOT to persist a
///   real-position contract via `set_active_position`.
pub struct HedgeOpenResult {
    pub total_notional: u64,
    pub signatures: Vec<solana_sdk::signature::Signature>,
    pub open_positions: Vec<(String, Pubkey, u64)>,
    pub sim_only: bool,
}

/// Submit hedge-leg short-open requests for each non-stable asset.
///
/// Returns a `HedgeOpenResult`. In `simulate_only` mode, signatures
/// will be empty and `total_notional` is 0 (audit-fix I5) — we still
/// build + whitelist-verify + simulate each ixn slice, but we don't
/// broadcast and don't credit notional.
///
/// On per-asset failure (build error, sim error, send error), we log
/// + skip that asset and continue. The composing caller treats the
/// final `hedge_notional_usd` as authoritative; if it's < expected,
/// the orchestrator can re-Assign with a smaller notional.
pub async fn open_short_requests(
    ctx: &DispatchCtx,
    payload: &AssignHedgedJlp,
    delta: &PortfolioDelta,
) -> Result<HedgeOpenResult> {
    let hedge_short_usd = compute_hedge_short_usd(payload, delta);
    if hedge_short_usd < MIN_HEDGE_NOTIONAL_USD {
        info!(
            hedge_short_usd,
            "hedge_short_usd below MIN_HEDGE_NOTIONAL_USD — skipping hedge leg"
        );
        return Ok(HedgeOpenResult {
            total_notional: 0,
            signatures: vec![],
            open_positions: vec![],
            sim_only: ctx.simulate_only,
        });
    }

    let allocations = allocate_per_asset(hedge_short_usd, delta);
    if allocations.is_empty() {
        info!("no per-asset slice met MIN_HEDGE_NOTIONAL_USD — skipping hedge leg");
        return Ok(HedgeOpenResult {
            total_notional: 0,
            signatures: vec![],
            open_positions: vec![],
            sim_only: ctx.simulate_only,
        });
    }

    let mut signatures = Vec::new();
    let mut total_notional = 0u64;
    let mut open_positions: Vec<(String, Pubkey, u64)> = Vec::new();

    // Audit fix 9: prefer the live-loaded pool when available; only
    // fall back to synthetic on devnet boot.
    let pool: PoolMeta = match &ctx.pool {
        Some(p) => (**p).clone(),
        None => synthetic_pool(),
    };

    // Use unix-seconds as the request counter base. Each per-asset
    // request gets `counter + i` so concurrent allocations don't
    // collide on the PositionRequest PDA derivation.
    let counter_base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    for (i, (asset, asset_share)) in allocations.into_iter().enumerate() {
        let counter = counter_base.wrapping_add(i as u64);

        // Audit fix 9: select real custody from the live pool when
        // present; fall back to synthetic stand-ins on devnet.
        let position_custody = pool
            .custody_for_mint(&asset.mint)
            .cloned()
            .unwrap_or_else(|| synthetic_custody(asset.mint, false));
        let collateral_custody = pool
            .custody_for_mint(&USDC_MINT)
            .cloned()
            .unwrap_or_else(|| synthetic_custody(USDC_MINT, true));
        if let Err(e) = validate_custody_not_synthetic(
            &position_custody,
            &format!("hedge open ({}) position-custody", asset.label),
            ctx.simulate_only,
        ) {
            warn!(
                asset = asset.label,
                ?e,
                "synthetic custody hard-stop on submit"
            );
            continue;
        }
        if let Err(e) = validate_custody_not_synthetic(
            &collateral_custody,
            &format!("hedge open ({}) collateral-custody", asset.label),
            ctx.simulate_only,
        ) {
            warn!(
                asset = asset.label,
                ?e,
                "synthetic custody hard-stop on submit"
            );
            continue;
        }

        match build_short_request_ixns_for_asset(
            ctx,
            &pool,
            &position_custody,
            &collateral_custody,
            &asset,
            asset_share,
            counter,
        ) {
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

                // Pre-derive the position PDA so we can record it on
                // submit success (audit-fix C1/C2).
                let user = ctx.wallet.pubkey();
                let position = derive_position(
                    &user,
                    &pool.pool,
                    &position_custody.address,
                    &collateral_custody.address,
                    PerpSide::Short,
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
                                // Audit-fix I5: do NOT credit
                                // total_notional in sim mode. Sim-only
                                // Reports surface hedge_notional_usdc=0.
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
                            // Audit-fix I5: credit only on submit success.
                            total_notional = total_notional.saturating_add(asset_share);
                            // Audit-fix C1/C2: record real position +
                            // counter so the unwind path can derive a
                            // matching close-request PDA.
                            open_positions.push((asset.label.to_string(), position, counter));
                        }
                        Err(e) => {
                            warn!(
                                asset = asset.label,
                                ?e,
                                "hedge build_sign_send failed; not crediting notional"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                warn!(
                    asset = asset.label,
                    ?e,
                    "build_short_request_ixns_for_asset failed"
                );
            }
        }
    }

    Ok(HedgeOpenResult {
        total_notional,
        signatures,
        open_positions,
        sim_only: ctx.simulate_only,
    })
}

/// Audit-fix C3: refuse to sign when CustodyMeta has synthetic
/// placeholder pubkeys. Any of `token_account`, `pythnet_price_account`,
/// or `doves_price_account` equal to the custody's own `address` is
/// treated as synthetic — true on the M6/M8 synthetic stand-ins, false
/// once a real on-chain custody loader populates the fields.
///
/// Behavior depends on `simulate_only`:
///   - `true`: log a warning and proceed (sim is informational; the
///     synthetic data will fail clean inside Solana's account validation).
///   - `false`: bail with the error message. The composing caller
///     surfaces this as `error_code=6` (build/validate failure) in the
///     Report, refusing to sign on mainnet first-test before the real
///     custody loader lands.
pub fn validate_custody_not_synthetic(
    custody: &CustodyMeta,
    operation: &str,
    simulate_only: bool,
) -> Result<()> {
    let synthetic = custody.token_account == custody.address
        || custody.pythnet_price_account == custody.address
        || custody.doves_price_account == custody.address;
    if synthetic {
        let msg = format!(
            "{} CustodyMeta has synthetic placeholder pubkeys (token_account/oracle == \
             custody address). Real custody loader must be wired before mainnet submit. \
             Refusing to sign.",
            operation
        );
        if simulate_only {
            warn!(
                operation,
                "synthetic custody detected — proceeding in sim-only mode"
            );
            Ok(())
        } else {
            anyhow::bail!(msg)
        }
    } else {
        Ok(())
    }
}

/// Build the per-asset short-open ixn slice. Audit fix 9: takes real
/// `CustodyMeta`s from the live pool (or synthetic fallback on devnet
/// boot — gated by the audit-fix C3 hard-stop before submit).
#[allow(clippy::too_many_arguments)]
fn build_short_request_ixns_for_asset(
    ctx: &DispatchCtx,
    pool: &PoolMeta,
    position_custody: &CustodyMeta,
    collateral_custody: &CustodyMeta,
    asset: &AssetSlice,
    notional_usd: u64,
    counter: u64,
) -> Result<Vec<Instruction>> {
    let user = ctx.wallet.pubkey();

    let collateral_amount = notional_usd / HEDGE_LEVERAGE;

    let position = derive_position(
        &user,
        &pool.pool,
        &position_custody.address,
        &collateral_custody.address,
        PerpSide::Short,
    );
    // Audit fix 3: PositionRequest PDA includes the request_change byte.
    let position_request = derive_position_request(&position, counter, RequestChange::Increase);

    // Audit fix 8: slippage is a 6-decimal USD mark price, not bps.
    // For a Short open: pass `mark * (1 + buffer)`. 1% buffer is
    // generous and matches the SDK example.
    let mark = sim_mark_price_micro_usd(asset.label);
    let price_slippage_micro_usd = mark + mark / 100;

    let ixs = create_increase_position_request_ix(
        &user,
        pool,
        position_custody,
        collateral_custody,
        &position,
        &position_request,
        notional_usd,
        collateral_amount,
        PerpSide::Short,
        price_slippage_micro_usd,
        counter,
    )
    .context("build create_increase_position_market_request ix")?;

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
            sol_usd: 50_000_000, // $50
            eth_usd: 30_000_000, // $30
            btc_usd: 20_000_000, // $20
            stable_usd: 100_000_000,
            total_usd: 200_000_000,
            long_exposure_bps: 5_000, // 50%
        }
    }

    #[test]
    fn hedge_short_usd_target_zero_means_full_neutralize() {
        // M9 corrected formula: target_delta_bps=0 → target_net=0
        //   → hedge = current_long.
        // current_long = 50 + 30 + 20 = 100M micro-USD
        let p = assign(0);
        let d = delta_balanced();
        let h = compute_hedge_short_usd(&p, &d);
        assert_eq!(
            h, 100_000_000,
            "target=0 must fully neutralize current long"
        );
    }

    #[test]
    fn hedge_short_usd_negative_target_means_extra_short_bias() {
        // target_delta_bps=-10_000 → target_net = -200M (full inverse).
        //   hedge = current_long(100M) - max(-200M, 0)(0) + max(200M, 0)(200M) = 300M
        // i.e. fully neutralize current long AND open another total_usd
        // worth of shorts (max short bias).
        let p = assign(-10_000);
        let d = delta_balanced();
        let h = compute_hedge_short_usd(&p, &d);
        assert_eq!(h, 300_000_000);
    }

    #[test]
    fn hedge_short_usd_small_long_bias_500_bps() {
        // total = 200M, target_delta_bps=+500 → target_net = +10M.
        //   hedge = current_long(100M) - 10M = 90M.
        let p = assign(500);
        let d = delta_balanced();
        let h = compute_hedge_short_usd(&p, &d);
        assert_eq!(h, 90_000_000);
    }

    #[test]
    fn hedge_short_usd_small_short_bias_neg_500_bps() {
        // total = 200M, target_delta_bps=-500 → target_net = -10M.
        //   hedge = current_long(100M) - 0 + 10M = 110M.
        let p = assign(-500);
        let d = delta_balanced();
        let h = compute_hedge_short_usd(&p, &d);
        assert_eq!(h, 110_000_000);
    }

    #[test]
    fn hedge_short_usd_max_long_bias_returns_zero_when_already_under() {
        // target = +10_000 (full long bias) → target_net = total = 200M.
        //   hedge = max(current_long(100M) - 200M, 0) = 0. (Note: in
        //   the wild this is unreachable since current_long <= total
        //   by construction, but the formula handles it cleanly.)
        let p = assign(10_000);
        let d = delta_balanced();
        let h = compute_hedge_short_usd(&p, &d);
        assert_eq!(h, 0);
    }

    #[test]
    fn hedge_short_usd_partial_target_neg_5000_bps() {
        // target=-5_000 (50% short bias of total=200M):
        //   target_net = -100M
        //   hedge = current_long(100M) + 100M = 200M
        let p = assign(-5_000);
        let d = delta_balanced();
        let h = compute_hedge_short_usd(&p, &d);
        assert_eq!(h, 200_000_000);
    }

    #[test]
    fn hedge_short_usd_below_threshold_returns_zero_after_filter() {
        // total = 1005 micro-USD, current_long = 5 micro-USD, target=0
        //   → hedge = 5 (below MIN_HEDGE_NOTIONAL_USD; filtered later).
        let mut d = delta_balanced();
        d.sol_usd = 5;
        d.eth_usd = 0;
        d.btc_usd = 0;
        d.stable_usd = 1000;
        d.total_usd = 1005;
        let p = assign(0);
        let h = compute_hedge_short_usd(&p, &d);
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

    // ── Audit-fix C3: synthetic-custody guard ───────────────────────────

    #[test]
    fn synthetic_custody_rejected_in_submit_mode() {
        // Default synthetic_custody() helper produces token_account ==
        // address (and oracle fields == address). validate_*() must
        // refuse to sign in submit mode.
        let synthetic = synthetic_custody(WSOL_MINT, false);
        let r = validate_custody_not_synthetic(&synthetic, "test", /*simulate_only*/ false);
        assert!(r.is_err(), "synthetic custody must hard-stop submit mode");
        let msg = format!("{:?}", r.unwrap_err());
        assert!(
            msg.contains("synthetic placeholder"),
            "error message must surface the synthetic-pubkey reason: {msg}"
        );
    }

    #[test]
    fn synthetic_custody_warned_in_sim_mode() {
        // In simulate_only mode, the same synthetic custody passes
        // (warn-only). Sim-only is informational; account validation
        // will reject the synthetic data downstream.
        let synthetic = synthetic_custody(WSOL_MINT, false);
        let r = validate_custody_not_synthetic(&synthetic, "test", /*simulate_only*/ true);
        assert!(r.is_ok(), "synthetic custody must warn-only in sim mode");
    }

    #[test]
    fn non_synthetic_custody_passes() {
        // A custody with distinct (non-self) pubkeys for token_account
        // and oracles is treated as real and passes both modes.
        let real = CustodyMeta {
            address: Pubkey::new_unique(),
            mint: WSOL_MINT,
            token_account: Pubkey::new_unique(),
            pythnet_price_account: Pubkey::new_unique(),
            doves_price_account: Pubkey::new_unique(),
            decimals: 9,
            is_stable: false,
        };
        assert!(validate_custody_not_synthetic(&real, "test", false).is_ok());
        assert!(validate_custody_not_synthetic(&real, "test", true).is_ok());
    }

    #[test]
    fn validate_detects_each_synthetic_field_independently() {
        // Pin: any one of the three address fields equal to the
        // custody's own pubkey is sufficient to flag synthetic.
        let base_addr = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        for (label, custody) in [
            (
                "token_account",
                CustodyMeta {
                    address: base_addr,
                    mint: WSOL_MINT,
                    token_account: base_addr, // synthetic
                    pythnet_price_account: other,
                    doves_price_account: other,
                    decimals: 9,
                    is_stable: false,
                },
            ),
            (
                "pythnet_price_account",
                CustodyMeta {
                    address: base_addr,
                    mint: WSOL_MINT,
                    token_account: other,
                    pythnet_price_account: base_addr, // synthetic
                    doves_price_account: other,
                    decimals: 9,
                    is_stable: false,
                },
            ),
            (
                "doves_price_account",
                CustodyMeta {
                    address: base_addr,
                    mint: WSOL_MINT,
                    token_account: other,
                    pythnet_price_account: other,
                    doves_price_account: base_addr, // synthetic
                    decimals: 9,
                    is_stable: false,
                },
            ),
        ] {
            let r = validate_custody_not_synthetic(&custody, "test", /*sim*/ false);
            assert!(r.is_err(), "field {label} must mark custody as synthetic");
        }
    }
}
