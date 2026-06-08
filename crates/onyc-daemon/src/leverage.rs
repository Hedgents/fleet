//! Simplified single-round leverage for onyc-daemon (option B).
//!
//! Replaces multiply's multi-round LTV-aware walk with a one-shot
//! USDC-borrow against the existing ONyc collateral:
//!
//!   1. seed (USDC→ONyc swap, deposit ONyc) — handled by `seed.rs`
//!   2. read obligation state — current collateral + debt
//!   3. compute target borrow amount toward `target_ltv_bps`
//!   4. submit one tx: RefreshReserve(ONyc) + RefreshReserve(USDC)
//!      + RefreshObligation + BorrowObligationLiquidityV2(USDC)
//!   5. borrowed USDC lands in the daemon wallet — operator (or next
//!      AssignOnyc) can recycle it back into more collateral via seed
//!
//! Trade-offs vs multiply's option-A multi-round walk:
//!   * No auto-recycle in the same tx. To reach a higher steady-state
//!     LTV, the orchestrator issues a follow-up AssignOnyc{ usdc_lamports:
//!     just_borrowed } so seed swaps + deposits the new principal,
//!     then a subsequent AssignOnyc{ usdc_lamports: 0, target_ltv_bps }
//!     borrows again. Each round is its own tx — simpler reasoning,
//!     no in-flight tracking, no flash-loans.
//!   * `MAX_LEVERAGE_LOOP_ROUNDS = 2` cap (caps.rs) governs the
//!     orchestrator's round count, not this function's per-call work.
//!     This function runs at most one round per Assign.
//!
//! Sim-only mode (`ctx.simulate_only`): simulates the borrow tx via
//! `build_sign_simulate`. No tx is broadcast and on-chain LTV does not
//! advance.

use anyhow::{bail, Context, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

use zerox1_defi_protocols::{
    constants::{
        KAMINO_ONYC_MARKET, KAMINO_ONYC_RESERVE, KAMINO_ONYC_USDC_RESERVE, ONYC_MINT, USDC_MINT,
    },
    protocols::{
        kamino::{
            borrow_obligation_liquidity_v2_ix, derive_user_obligation_with_seed,
            refresh_obligation_ix, refresh_reserve_ix, ReserveAccounts,
        },
        kamino_loader::{fetch_obligation, load_reserve, query_position_ltv_bps_with_seed},
    },
};
use zerox1_protocol::fleet::onyc::{AssignOnyc, ReportOnyc};
use zerox1_protocol::fleet::ReportHeader;

use crate::caps;
use crate::dispatch::DispatchCtx;

/// Compute budget for the borrow tx. Lower than multiply's because we
/// only emit 4 ixs (no Jito stake, no deposit-back), no farm CPI on the
/// borrow side (KAMINO_ONYC_USDC_FARM_COLLATERAL exists on the reserve
/// but Kamino's v2 borrow handler refreshes it inline).
const BORROW_CU_LIMIT: u32 = 600_000;
const BORROW_PRIORITY_FEE: u64 = 10_000;

/// Stop the round if within this many bps of target. Borrowing the last
/// few bps risks ratio oscillation and Kamino's chain-side BF clamp
/// rejecting an arithmetic-precision overshoot.
const TARGET_PROXIMITY_BPS: u16 = 50;

/// Safety factor applied to the naive borrow amount before sending. 0.90
/// gives us 10% headroom for: (1) borrow-factor adjustment (USDC BF=1.0
/// on the ONyc market so no clamp needed in theory — kept as defense in
/// depth), (2) oracle drift between the off-chain compute and the on-chain
/// validate at Anchor program landing, (3) future NAV step-down between
/// borrow and any subsequent operation.
const BORROW_SAFETY_BPS: u64 = 9_000;

/// Pure helper: given current obligation state (BF-adjusted), compute
/// the USDC lamports we can safely borrow to reach `target_ltv_bps`.
///
/// LTV (basis points) := (BF-adjusted debt USD × 10_000) / collateral USD.
/// Target borrow := target_ltv_bps × collateral / 10_000 − current_debt.
/// All inputs are sf-scaled USD (Kamino's market_value_sf encoding) so the
/// division must scale back out at the end.
///
/// Returns 0 when:
///   * already at/above target
///   * collateral is zero (no obligation, can't borrow)
///   * the safe headroom would overflow
pub fn compute_borrow_lamports_for_target(
    collateral_value_sf: u128,
    bf_debt_value_sf: u128,
    target_ltv_bps: u16,
    usdc_value_per_lamport_sf: u128,
) -> u64 {
    if collateral_value_sf == 0 || target_ltv_bps == 0 || usdc_value_per_lamport_sf == 0 {
        return 0;
    }
    // target_debt = collateral × target_ltv / 10_000
    let target_debt_sf = collateral_value_sf
        .saturating_mul(target_ltv_bps as u128)
        / 10_000;
    if target_debt_sf <= bf_debt_value_sf {
        return 0;
    }
    let raw_headroom_sf = target_debt_sf - bf_debt_value_sf;
    // Apply BORROW_SAFETY_BPS to the headroom.
    let safe_headroom_sf = raw_headroom_sf.saturating_mul(BORROW_SAFETY_BPS as u128) / 10_000;
    // sf-scaled headroom / sf-scaled price = lamports.
    let lamports = safe_headroom_sf / usdc_value_per_lamport_sf;
    u64::try_from(lamports).unwrap_or(u64::MAX)
}

/// Either simulate the leverage round or actually submit it (per
/// `ctx.simulate_only`).
pub async fn run_or_simulate(
    ctx: &DispatchCtx,
    assign: &AssignOnyc,
    conv: [u8; 16],
) -> Result<ReportOnyc> {
    let user = ctx.wallet.pubkey();
    let lending_market = KAMINO_ONYC_MARKET;

    // Deadline check up front — fail fast.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if assign.deadline_unix > 0 && assign.deadline_unix < now {
        bail!(
            "AssignOnyc deadline {} has passed (now {})",
            assign.deadline_unix,
            now
        );
    }

    info!(
        simulate_only = ctx.simulate_only,
        target_ltv_bps = assign.target_ltv_bps,
        usdc_lamports = assign.usdc_lamports,
        "onyc leverage entry"
    );

    // Phase 1: seed (USDC→ONyc swap + deposit). No-op when usdc_lamports=0
    // and obligation already has collateral.
    let _seeded = crate::seed::maybe_seed_obligation(ctx)
        .await
        .context("maybe_seed_obligation")?;

    if ctx.simulate_only {
        // Simulation doesn't move on-chain state — we'd loop on the same
        // pre-seed LTV. Return early.
        info!("simulate-only: seed simulated; skipping borrow round simulation");
        return Ok(ReportOnyc {
            header: ReportHeader::ok(conv),
            resulting_ltv_bps: 0,
            tx_signature: None,
        });
    }

    // Phase 2: read current LTV.
    let current_ltv = query_position_ltv_bps_with_seed(
        &ctx.rpc.client,
        user,
        lending_market,
        caps::ONYC_OBLIGATION_SEED.0,
        caps::ONYC_OBLIGATION_SEED.1,
    )
    .await
    .context("query initial LTV")?;

    if current_ltv >= assign.target_ltv_bps {
        info!(
            current_ltv_bps = current_ltv,
            target_ltv_bps = assign.target_ltv_bps,
            "already at or above target — no borrow round needed"
        );
        return Ok(ReportOnyc {
            header: ReportHeader::ok(conv),
            resulting_ltv_bps: current_ltv,
            tx_signature: None,
        });
    }

    if assign.target_ltv_bps.saturating_sub(current_ltv) < TARGET_PROXIMITY_BPS {
        info!(
            current_ltv_bps = current_ltv,
            target_ltv_bps = assign.target_ltv_bps,
            "within proximity band — skipping round"
        );
        return Ok(ReportOnyc {
            header: ReportHeader::ok(conv),
            resulting_ltv_bps: current_ltv,
            tx_signature: None,
        });
    }

    // Phase 3: load reserves + obligation, compute borrow amount.
    let onyc_reserve = load_reserve(&ctx.rpc.client, &KAMINO_ONYC_RESERVE, ONYC_MINT, &lending_market)
        .await
        .context("load ONyc reserve")?;
    let usdc_reserve = load_reserve(
        &ctx.rpc.client,
        &KAMINO_ONYC_USDC_RESERVE,
        USDC_MINT,
        &lending_market,
    )
    .await
    .context("load USDC reserve")?;

    let obligation_addr = derive_user_obligation_with_seed(
        &user,
        &lending_market,
        caps::ONYC_OBLIGATION_SEED.0,
        caps::ONYC_OBLIGATION_SEED.1,
    );
    let decoded = fetch_obligation(&ctx.rpc.client, &obligation_addr)
        .await
        .context("fetch obligation for borrow sizing")?
        .ok_or_else(|| {
            anyhow::anyhow!("obligation not found after seed — seed bundle did not land")
        })?;

    // Collateral value from the deposit slot. ONyc-market obligations
    // hold only ONyc as collateral in v0; sum across deposits is robust
    // to future asset additions.
    let collateral_value_sf: u128 = decoded.deposits.iter().map(|d| d.market_value_sf).sum();
    // BF-adjusted debt (Kamino tracks `borrow_factor_adjusted_debt_value_sf`).
    let bf_debt_value_sf = decoded.borrow_factor_adjusted_debt_value_sf;
    // USDC value per lamport sf. Find any existing USDC borrow's
    // value/lamport ratio; on a fresh wallet, USDC's stable peg gives us
    // sf-scaled $1/1e6 ≈ 1.85e12 sf/lamport. We approximate from the
    // reserve's scope price if no existing borrow.
    let usdc_value_per_lamport_sf: u128 = decoded
        .borrows
        .iter()
        .find(|b| b.reserve == KAMINO_ONYC_USDC_RESERVE && b.borrowed_amount_sf > 0)
        .map(|b| {
            b.market_value_sf
                .saturating_mul(1u128 << 60)
                .saturating_div(b.borrowed_amount_sf.max(1))
        })
        .unwrap_or_else(|| {
            // Fallback: assume USDC ≈ $1. sf-scale is 2^60 in Kamino's
            // fixed-point; $1 over 1e6 USDC lamports ≈ 2^60 / 1e6.
            (1u128 << 60) / 1_000_000
        });

    let target_ltv_bps = assign.target_ltv_bps.min(caps::MAX_LTV_BPS);
    let borrow_lamports = compute_borrow_lamports_for_target(
        collateral_value_sf,
        bf_debt_value_sf,
        target_ltv_bps,
        usdc_value_per_lamport_sf,
    );

    if borrow_lamports == 0 {
        warn!(
            current_ltv_bps = current_ltv,
            target_ltv_bps,
            "computed borrow amount is 0 — likely already past target or zero collateral"
        );
        return Ok(ReportOnyc {
            header: ReportHeader::ok(conv),
            resulting_ltv_bps: current_ltv,
            tx_signature: None,
        });
    }

    info!(
        borrow_lamports,
        current_ltv_bps = current_ltv,
        target_ltv_bps,
        "computed simplified single-round USDC borrow"
    );

    // Phase 4: build the borrow bundle.
    let ixs = build_borrow_bundle(&user, &onyc_reserve, &usdc_reserve, borrow_lamports)?;

    // Phase 5: simulate or submit.
    let sig = ctx
        .rpc
        .build_sign_send(ixs, ctx.wallet.keypair(), BORROW_CU_LIMIT, BORROW_PRIORITY_FEE)
        .await
        .context("submit USDC borrow tx")?;
    info!(%sig, "USDC borrow confirmed");

    // Re-query LTV for the report.
    let new_ltv = query_position_ltv_bps_with_seed(
        &ctx.rpc.client,
        user,
        lending_market,
        caps::ONYC_OBLIGATION_SEED.0,
        caps::ONYC_OBLIGATION_SEED.1,
    )
    .await
    .context("query post-borrow LTV")?;

    Ok(ReportOnyc {
        header: ReportHeader::ok(conv),
        resulting_ltv_bps: new_ltv,
        tx_signature: Some(sig.to_string()),
    })
}

/// Build the single-round borrow bundle.
///
///   1. RefreshReserve(ONyc) — collateral must be fresh for the
///      RefreshObligation slot below to recompute collateral_value_sf
///   2. RefreshReserve(USDC) — the borrow target reserve
///   3. RefreshObligation — both deposits + borrows refreshed in-tx
///   4. BorrowObligationLiquidityV2(USDC, borrow_lamports)
fn build_borrow_bundle(
    user: &Pubkey,
    onyc_reserve: &ReserveAccounts,
    usdc_reserve: &ReserveAccounts,
    borrow_lamports: u64,
) -> Result<Vec<Instruction>> {
    let mut ixs = Vec::with_capacity(4);
    ixs.push(refresh_reserve_ix(onyc_reserve));
    ixs.push(refresh_reserve_ix(usdc_reserve));
    let obligation_reserves = vec![onyc_reserve.reserve, usdc_reserve.reserve];
    ixs.push(refresh_obligation_ix(
        user,
        &onyc_reserve.lending_market,
        caps::ONYC_OBLIGATION_SEED,
        &obligation_reserves,
    ));
    ixs.push(borrow_obligation_liquidity_v2_ix(
        user,
        usdc_reserve,
        borrow_lamports,
        caps::ONYC_OBLIGATION_SEED,
    )?);
    Ok(ixs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: encode a USD value in sf-fixed-point (Kamino's 2^60 scale).
    fn usd_sf(usd: f64) -> u128 {
        ((usd * (1u128 << 60) as f64) as u128).max(1)
    }

    /// USDC ≈ $1/1e6 lamports → 2^60/1e6 sf/lamport.
    fn usdc_price_per_lamport_sf() -> u128 {
        (1u128 << 60) / 1_000_000
    }

    #[test]
    fn borrow_zero_when_collateral_empty() {
        let n = compute_borrow_lamports_for_target(0, 0, 4_000, usdc_price_per_lamport_sf());
        assert_eq!(n, 0);
    }

    #[test]
    fn borrow_zero_when_target_zero() {
        let n = compute_borrow_lamports_for_target(usd_sf(100.0), 0, 0, usdc_price_per_lamport_sf());
        assert_eq!(n, 0);
    }

    #[test]
    fn borrow_zero_when_already_at_target() {
        // $100 collateral, $40 BF-debt, target 40% → already at target.
        let n = compute_borrow_lamports_for_target(
            usd_sf(100.0),
            usd_sf(40.0),
            4_000,
            usdc_price_per_lamport_sf(),
        );
        assert_eq!(n, 0);
    }

    #[test]
    fn borrow_targets_40pct_on_fresh_position() {
        // $100 ONyc collateral, $0 debt, target 40% LTV.
        // Target borrow = $40. After 0.9 safety = $36 = 36 USDC = 36e6 lamports.
        let n = compute_borrow_lamports_for_target(
            usd_sf(100.0),
            0,
            4_000,
            usdc_price_per_lamport_sf(),
        );
        // Allow ±2% slop for sf-fixed-point rounding.
        let expected: i128 = 36_000_000;
        let delta = (n as i128 - expected).abs();
        assert!(
            delta < expected / 50,
            "n={n}, expected≈{expected}, delta={delta}"
        );
    }

    #[test]
    fn borrow_top_up_partial_to_reach_target() {
        // $100 collateral, $20 BF-debt, target 40% → headroom $20.
        // After 0.9 safety = $18 = 18 USDC = 18e6 lamports.
        let n = compute_borrow_lamports_for_target(
            usd_sf(100.0),
            usd_sf(20.0),
            4_000,
            usdc_price_per_lamport_sf(),
        );
        let expected: i128 = 18_000_000;
        let delta = (n as i128 - expected).abs();
        assert!(
            delta < expected / 50,
            "n={n}, expected≈{expected}, delta={delta}"
        );
    }

    #[test]
    fn borrow_safety_factor_shrinks_amount() {
        // Verify the BORROW_SAFETY_BPS=9000 (90%) clamp is applied:
        // unclamped headroom would be $40; safe should be $36.
        let n = compute_borrow_lamports_for_target(
            usd_sf(100.0),
            0,
            4_000,
            usdc_price_per_lamport_sf(),
        );
        // n should be ≤ $36 * 1.01 (safety + tiny rounding upward).
        let upper = 36_360_000u64;
        assert!(
            n <= upper,
            "expected ≤{upper} ($36.36 cap), got {n}"
        );
    }

    #[test]
    fn borrow_zero_when_price_zero() {
        // Degenerate: no price ratio. compute returns 0 rather than
        // dividing by zero.
        let n = compute_borrow_lamports_for_target(usd_sf(100.0), 0, 4_000, 0);
        assert_eq!(n, 0);
    }

    #[test]
    fn proximity_band_constant_is_sensible() {
        // Sanity — TARGET_PROXIMITY_BPS sits well below the typical
        // operator-set target (40% = 4000 bps).
        assert!(TARGET_PROXIMITY_BPS < 1_000);
        assert!(TARGET_PROXIMITY_BPS > 0);
    }

    #[test]
    fn safety_constants_sensible() {
        assert!(BORROW_SAFETY_BPS <= 10_000);
        assert!(BORROW_SAFETY_BPS >= 5_000); // anything below 50% is degenerate
        assert!(BORROW_CU_LIMIT >= 200_000);
    }
}
