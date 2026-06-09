//! Position telemetry for multiply-daemon — periodic snapshots + JSONL log.
//!
//! Each tick we:
//!   1. Read the on-chain Kamino obligation (real position when deployed;
//!      zero in simulate-only mode).
//!   2. Fetch live fleet rates (jitoSOL APY, USDC borrow, etc.) and compute
//!      the multiply strategy's leveraged net yield.
//!   3. Accumulate paper P&L: principal × net_apr × elapsed / year.
//!   4. Write a JSONL line with `total_aum_usdc` so the dashboard P&L
//!      chart picks it up via `pnl_row_to_usd`.
//!
//! Dashboard reads `total_aum_usdc` first, then falls back to
//! `net_equity_uusdc / 1e6`, so the telemetry drives the P&L chart.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use zerox1_defi_protocols::constants::{KAMINO_ONYC_RESERVE, KAMINO_ONYC_USDC_RESERVE};
use zerox1_defi_runtime::fleet_rates::{fetch_fleet_rates, FleetRates};
use zerox1_defi_runtime::rpc::RpcContext;

const SECS_PER_YEAR: f64 = 31_536_000.0;

/// Kamino's scaled-fraction divisor — divide an sf value by 2^60 to get
/// the real USD value.
const SF_DIVISOR_SHIFT: u32 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionSnapshot {
    pub timestamp_unix: u64,
    /// Total collateral USD × 1e6 (µUSD) — from on-chain obligation.
    pub deposited_uusdc: i64,
    /// Total debt USD × 1e6 (µUSD).
    pub borrowed_uusdc: i64,
    /// Net equity µUSD (deposited − borrowed). Negative if underwater.
    pub net_equity_uusdc: i64,

    // ── Paper trading P&L ────────────────────────────────────────────
    /// Notional paper principal in USD (set via --paper-principal-usdc-lamports).
    pub paper_principal_usdc: f64,
    /// Seconds since daemon start — drives P&L accumulation.
    pub paper_elapsed_secs: u64,
    /// ONyc net APR, bps. v0.5.0 placeholder: ONyc base NAV growth
    /// estimated at 1100 bps (~11%) minus the live USDC borrow rate
    /// × current LTV. The dashboard's `apr_field = "onyc_net_apr_bps"`
    /// looks this up to populate the strategy card's current APR. A
    /// proper NAV-derived rate (driven by Chainlink Data Streams +
    /// Apex attestation deltas) lands in a future patch.
    pub onyc_net_apr_bps: u16,
    /// Accumulated simulated earnings since daemon start.
    pub paper_earned_usdc: f64,
    /// Per-day earnings at current APR.
    pub paper_daily_rate_usdc: f64,
    /// Per-year earnings at current APR.
    pub paper_annual_rate_usdc: f64,
    /// paper_principal + paper_earned — picked up by dashboard P&L chart.
    pub total_aum_usdc: f64,
    // For transparency — raw rates used in the computation.
    pub jitosol_apy_pct: f64,
    pub usdc_borrow_pct: f64,
    /// v0.4.11: the SOL borrow rate (multiply's actual debt cost — see
    /// v0.4.7). Was implicit in `onyc_net_apr_bps` after v0.4.7
    /// switched the formula to use `kamino_sol_borrow_pct`, but the
    /// field itself was never surfaced. Surfacing it now lets the
    /// dashboard show "@ X% cost" alongside the debt $ on the
    /// strategy card. `usdc_borrow_pct` is kept for backward
    /// compatibility (it's still emitted and consumed by older
    /// tooling); the two fields are independent rates Kamino quotes
    /// for the same market.
    pub sol_borrow_pct: f64,
}

/// Convert an sf-scaled u128 to µUSD (i64).
fn sf_to_uusdc(sf: u128) -> i64 {
    let scaled = sf.saturating_mul(1_000_000);
    let uusdc = scaled >> SF_DIVISOR_SHIFT;
    uusdc.min(i64::MAX as u128) as i64
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub async fn snapshot(
    rpc: &Arc<RpcContext>,
    user: Pubkey,
    lending_market: Pubkey,
    start_ts: u64,
    paper_principal_usdc: f64,
    simulate_only: bool,
) -> Result<PositionSnapshot> {
    use zerox1_defi_protocols::protocols::kamino::derive_user_obligation_with_seed;
    use zerox1_defi_protocols::protocols::kamino_loader::fetch_obligation;

    // Multiply's obligation lives under the (0, 1) seed — see
    // `caps::ONYC_OBLIGATION_SEED`. Telemetry must read from the same
    // PDA the leverage loop writes to.
    let obligation_addr = derive_user_obligation_with_seed(
        &user,
        &lending_market,
        crate::caps::ONYC_OBLIGATION_SEED.0,
        crate::caps::ONYC_OBLIGATION_SEED.1,
    );

    // Fetch on-chain position + live rates in parallel.
    let (decoded, rates) = tokio::join!(
        fetch_obligation(&rpc.client, &obligation_addr),
        fetch_fleet_rates(),
    );
    let decoded = decoded.context("fetch obligation for pnl snapshot")?;

    let now = now_unix();
    let elapsed_secs = now.saturating_sub(start_ts);

    // v0.5.0: ONyc base NAV growth ~11% (1100 bps). When leverage is
    // applied, subtract USDC borrow × LTV. Formula:
    //   net = ONYC_BASE - usdc_borrow_pct × ltv
    // v0.5.9: always compute LTV via per-position NAV pricing on the
    // ONyc-market obligation. Kamino populates the aggregate
    // `deposited_value_sf` for the ONyc deposit (Chainlink price IS
    // resolved at the reserve level by RefreshReserve) but
    // `borrowed_assets_market_value_sf` stays 0 until a later
    // RefreshObligation runs the ONyc-market USDC reserve through
    // the same RefreshReserve dance — and we don't run an idle
    // RefreshObligation tick. Read borrowed_amount_sf per borrow
    // slot directly (always available, never lags) and re-price the
    // ONyc deposit ourselves so the LTV is always coherent with the
    // landing page's USDC-borrow-cost calculation.
    const ONYC_BASE_NAV_GROWTH_BPS: u16 = 1100;
    const ONYC_NAV_MICRO_USD: u128 = 1_110_000; // $1.11 placeholder
    const ONYC_DECIMALS: u32 = 9;
    let ltv_frac: f64 = match &decoded {
        Some(o) => {
            let pow10 = 10u128.pow(ONYC_DECIMALS);
            let coll_micro_usd: u128 = o
                .deposits
                .iter()
                .filter(|d| d.reserve == KAMINO_ONYC_RESERVE)
                .map(|d| {
                    (d.deposited_amount as u128)
                        .saturating_mul(ONYC_NAV_MICRO_USD)
                        .saturating_div(pow10)
                })
                .sum();
            // borrowed_amount_sf >> 60 gives raw USDC lamports
            // (6 dp). USDC ≈ $1, so lamports == micro-USD 1:1.
            let borrow_micro_usd: u128 = o
                .borrows
                .iter()
                .filter(|b| b.reserve == KAMINO_ONYC_USDC_RESERVE)
                .map(|b| (b.borrowed_amount_sf >> 60))
                .sum();
            if coll_micro_usd > 0 {
                borrow_micro_usd as f64 / coll_micro_usd as f64
            } else {
                0.0
            }
        }
        _ => 0.0,
    };
    let borrow_cost_bps =
        (rates.kamino_usdc_borrow_pct.max(0.0) * 100.0 * ltv_frac).round() as i32;
    let net_apr_bps = (ONYC_BASE_NAV_GROWTH_BPS as i32 - borrow_cost_bps).max(0) as u16;

    // Paper-mode synthetic accumulation: only computed when
    // `simulate_only` is true. In live mode every paper_* field +
    // total_aum_usdc is forced to 0 so the JSONL row carries an honest
    // signal (no synthetic baseline contaminating the telemetry feed
    // the dashboard's /pnl reads).
    let (paper_principal, elapsed, earned, daily, annual, total_aum) = if simulate_only {
        let apr_frac = net_apr_bps as f64 / 10_000.0;
        let annual = paper_principal_usdc * apr_frac;
        let earned = annual * (elapsed_secs as f64 / SECS_PER_YEAR);
        let daily = annual / 365.0;
        let total_aum = paper_principal_usdc + earned;
        (
            paper_principal_usdc,
            elapsed_secs,
            earned,
            daily,
            annual,
            total_aum,
        )
    } else {
        (0.0, 0, 0.0, 0.0, 0.0, 0.0)
    };

    // v0.5.9: same NAV-fallback story for the displayed dep/bor —
    // borrowed_assets_market_value_sf stays 0 between RefreshObligation
    // ticks on the ONyc market, so use the per-position numbers
    // (Chainlink-priced ONyc deposit, USDC borrow @ $1) directly.
    let (dep, bor) = match decoded {
        None => (0, 0),
        Some(o) => {
            let pow10 = 10u128.pow(ONYC_DECIMALS);
            let dep_micro_usd: u128 = o
                .deposits
                .iter()
                .filter(|d| d.reserve == KAMINO_ONYC_RESERVE)
                .map(|d| {
                    (d.deposited_amount as u128)
                        .saturating_mul(ONYC_NAV_MICRO_USD)
                        .saturating_div(pow10)
                })
                .sum();
            let bor_micro_usd: u128 = o
                .borrows
                .iter()
                .filter(|b| b.reserve == KAMINO_ONYC_USDC_RESERVE)
                .map(|b| (b.borrowed_amount_sf >> 60))
                .sum();
            (
                dep_micro_usd.min(i64::MAX as u128) as i64,
                bor_micro_usd.min(i64::MAX as u128) as i64,
            )
        }
    };

    Ok(PositionSnapshot {
        timestamp_unix: now,
        deposited_uusdc: dep,
        borrowed_uusdc: bor,
        net_equity_uusdc: dep.saturating_sub(bor),
        paper_principal_usdc: paper_principal,
        paper_elapsed_secs: elapsed,
        onyc_net_apr_bps: net_apr_bps,
        paper_earned_usdc: earned,
        paper_daily_rate_usdc: daily,
        paper_annual_rate_usdc: annual,
        total_aum_usdc: total_aum,
        jitosol_apy_pct: rates.jitosol_apy_pct,
        usdc_borrow_pct: rates.kamino_usdc_borrow_pct,
        sol_borrow_pct: rates.kamino_sol_borrow_pct,
    })
}

/// Append a snapshot to the JSONL log at `path`. Creates the file if needed.
pub fn append_to_log(path: &Path, snap: &PositionSnapshot) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open pnl log at {}", path.display()))?;
    writeln!(f, "{}", serde_json::to_string(snap)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sf_conversion_samples() {
        assert_eq!(sf_to_uusdc(0), 0);
        assert_eq!(sf_to_uusdc(1u128 << 60), 1_000_000);
        assert_eq!(sf_to_uusdc(100u128 * (1u128 << 60)), 100_000_000);
    }

    #[test]
    fn snapshot_round_trips_via_json() {
        let snap = PositionSnapshot {
            timestamp_unix: 1_714_800_000,
            deposited_uusdc: 50_000_000,
            borrowed_uusdc: 30_000_000,
            net_equity_uusdc: 20_000_000,
            paper_principal_usdc: 50_000.0,
            paper_elapsed_secs: 86400,
            onyc_net_apr_bps: 1322,
            paper_earned_usdc: 50_000.0 * 0.1322 / 365.0,
            paper_daily_rate_usdc: 50_000.0 * 0.1322 / 365.0,
            paper_annual_rate_usdc: 50_000.0 * 0.1322,
            total_aum_usdc: 50_000.0 + 50_000.0 * 0.1322 / 365.0,
            jitosol_apy_pct: 8.3,
            usdc_borrow_pct: 5.02,
            sol_borrow_pct: 7.58,
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("total_aum_usdc"));
        assert!(json.contains("onyc_net_apr_bps"));
        assert!(json.contains("sol_borrow_pct"));
        let back: PositionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.onyc_net_apr_bps, 1322);
        assert!((back.total_aum_usdc - snap.total_aum_usdc).abs() < 1e-9);
        assert!((back.sol_borrow_pct - 7.58).abs() < 1e-9);
    }
}
