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
    /// Multiply net APR, bps (jitoSOL × leverage − USDC borrow × debt).
    pub multiply_net_apr_bps: u16,
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
) -> Result<PositionSnapshot> {
    use zerox1_defi_protocols::protocols::kamino::derive_user_obligation;
    use zerox1_defi_protocols::protocols::kamino_loader::fetch_obligation;

    let obligation_addr = derive_user_obligation(&user, &lending_market);

    // Fetch on-chain position + live rates in parallel.
    let (decoded, rates) = tokio::join!(
        fetch_obligation(&rpc.client, &obligation_addr),
        fetch_fleet_rates(),
    );
    let decoded = decoded.context("fetch obligation for pnl snapshot")?;

    let now = now_unix();
    let elapsed_secs = now.saturating_sub(start_ts);

    let net_apr_bps = rates.multiply_net_apr_bps;
    let apr_frac = net_apr_bps as f64 / 10_000.0;
    let annual = paper_principal_usdc * apr_frac;
    let earned = annual * (elapsed_secs as f64 / SECS_PER_YEAR);
    let daily = annual / 365.0;
    let total_aum = paper_principal_usdc + earned;

    let (dep, bor) = match decoded {
        None => (0, 0),
        Some(o) => (
            sf_to_uusdc(o.deposited_value_sf),
            sf_to_uusdc(o.borrowed_assets_market_value_sf),
        ),
    };

    Ok(PositionSnapshot {
        timestamp_unix: now,
        deposited_uusdc: dep,
        borrowed_uusdc: bor,
        net_equity_uusdc: dep.saturating_sub(bor),
        paper_principal_usdc,
        paper_elapsed_secs: elapsed_secs,
        multiply_net_apr_bps: net_apr_bps,
        paper_earned_usdc: earned,
        paper_daily_rate_usdc: daily,
        paper_annual_rate_usdc: annual,
        total_aum_usdc: total_aum,
        jitosol_apy_pct: rates.jitosol_apy_pct,
        usdc_borrow_pct: rates.kamino_usdc_borrow_pct,
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
            multiply_net_apr_bps: 1322,
            paper_earned_usdc: 50_000.0 * 0.1322 / 365.0,
            paper_daily_rate_usdc: 50_000.0 * 0.1322 / 365.0,
            paper_annual_rate_usdc: 50_000.0 * 0.1322,
            total_aum_usdc: 50_000.0 + 50_000.0 * 0.1322 / 365.0,
            jitosol_apy_pct: 8.3,
            usdc_borrow_pct: 5.02,
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("total_aum_usdc"));
        assert!(json.contains("multiply_net_apr_bps"));
        let back: PositionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.multiply_net_apr_bps, 1322);
        assert!((back.total_aum_usdc - snap.total_aum_usdc).abs() < 1e-9);
    }
}
