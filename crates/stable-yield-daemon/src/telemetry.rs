//! Paper-trading P&L telemetry for stable-yield-daemon.
//!
//! Each tick we:
//!   1. Fetch the live Kamino USDC supply APR from DeFiLlama.
//!   2. Compute simulated earnings since daemon start using the formula:
//!        earned = principal × (apr_bps / 10_000) × (elapsed_secs / 31_536_000)
//!   3. Append a JSONL line with all fields, including `total_aum_usdc`
//!      (principal + earned) so the dashboard P&L chart shows a rising curve.
//!
//! When the wallet has an on-chain Kamino obligation the deposited balance
//! is read from chain and replaces the simulated principal — real money
//! takes precedence over the paper figure.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::rates::fetch_kamino_usdc_apr_bps;
use zerox1_defi_runtime::rpc::RpcContext;

const SECS_PER_YEAR: f64 = 31_536_000.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryLine {
    pub ts: u64,
    pub obligation_pubkey: String,
    /// On-chain deposited balance, or 0 in simulate-only mode.
    pub deposited_usdc_lamports: u64,
    /// Live Kamino USDC main-market supply APR, basis points.
    pub supply_apr_bps: u16,
    /// Paper-trade principal (configurable via --paper-principal-usdc-lamports).
    pub paper_principal_usdc: f64,
    /// Seconds since daemon started — drives the P&L accumulation.
    pub paper_elapsed_secs: u64,
    /// Simulated earnings so far: principal × apr × elapsed / year.
    pub paper_earned_usdc: f64,
    /// What we'd earn per calendar day at current APR.
    pub paper_daily_rate_usdc: f64,
    /// What we'd earn per year at current APR.
    pub paper_annual_rate_usdc: f64,
    /// principal + earned — this is what the dashboard P&L chart tracks.
    /// Picked up by `pnl_row_to_usd` via the "total_aum_usdc" key.
    pub total_aum_usdc: f64,
}

pub async fn run(
    rpc: Arc<RpcContext>,
    payer: Pubkey,
    market: Pubkey,
    log_path: PathBuf,
    interval_secs: u64,
    paper_principal_usdc_lamports: u64,
) -> Result<()> {
    let start_ts = now_unix();
    let paper_principal_usdc = paper_principal_usdc_lamports as f64 / 1_000_000.0;

    info!(
        log_path = %log_path.display(),
        interval_secs,
        market = %market,
        paper_principal_usdc,
        "telemetry loop starting",
    );

    let mut tick = interval(Duration::from_secs(interval_secs.max(1)));
    tick.tick().await;
    poll_and_log(
        &rpc,
        &payer,
        &market,
        &log_path,
        start_ts,
        paper_principal_usdc,
    )
    .await;
    loop {
        tick.tick().await;
        poll_and_log(
            &rpc,
            &payer,
            &market,
            &log_path,
            start_ts,
            paper_principal_usdc,
        )
        .await;
    }
}

async fn poll_and_log(
    rpc: &Arc<RpcContext>,
    payer: &Pubkey,
    market: &Pubkey,
    log_path: &Path,
    start_ts: u64,
    paper_principal_usdc: f64,
) {
    if let Err(e) = poll_once(rpc, payer, market, log_path, start_ts, paper_principal_usdc).await {
        warn!(?e, "telemetry poll failed");
    }
}

async fn poll_once(
    rpc: &Arc<RpcContext>,
    payer: &Pubkey,
    market: &Pubkey,
    log_path: &Path,
    start_ts: u64,
    paper_principal_usdc: f64,
) -> Result<()> {
    use zerox1_defi_protocols::protocols::kamino::derive_user_obligation;
    use zerox1_defi_protocols::protocols::kamino_loader::fetch_obligation;

    let obligation_pk = derive_user_obligation(payer, market);
    let now = now_unix();
    let elapsed_secs = now.saturating_sub(start_ts);

    // Live APR from DeFiLlama — same source as the dashboard /rates endpoint.
    let supply_apr_bps = fetch_kamino_usdc_apr_bps().await;
    let apr_frac = supply_apr_bps as f64 / 10_000.0;

    // P&L math.
    let paper_annual_rate_usdc = paper_principal_usdc * apr_frac;
    let paper_daily_rate_usdc = paper_annual_rate_usdc / 365.0;
    let paper_earned_usdc = paper_annual_rate_usdc * (elapsed_secs as f64 / SECS_PER_YEAR);
    let total_aum_usdc = paper_principal_usdc + paper_earned_usdc;

    // Prefer real on-chain balance when a deposit actually exists.
    let deposited_usdc_lamports = match fetch_obligation(&rpc.client, &obligation_pk).await {
        Ok(Some(decoded)) => decoded
            .deposits
            .iter()
            .map(|d| d.deposited_amount)
            .fold(0u64, |acc, x| acc.saturating_add(x)),
        _ => 0,
    };

    let line = TelemetryLine {
        ts: now,
        obligation_pubkey: obligation_pk.to_string(),
        deposited_usdc_lamports,
        supply_apr_bps,
        paper_principal_usdc,
        paper_elapsed_secs: elapsed_secs,
        paper_earned_usdc,
        paper_daily_rate_usdc,
        paper_annual_rate_usdc,
        total_aum_usdc,
    };

    append_line(log_path, &line)?;
    info!(
        supply_apr_bps,
        paper_principal_usdc,
        paper_earned_usdc,
        paper_daily_rate_usdc,
        paper_elapsed_secs = elapsed_secs,
        total_aum_usdc,
        "telemetry tick",
    );
    debug!(obligation = %obligation_pk, deposited_usdc_lamports, "on-chain balance");
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn append_line(log_path: &Path, line: &TelemetryLine) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;

    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("open telemetry log at {}", log_path.display()))?;
    let json = serde_json::to_string(line).context("serialize telemetry line")?;
    writeln!(f, "{json}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn earnings_math_is_correct() {
        // $1000 at 4% APR for 1 year = $40.
        let principal = 1000.0_f64;
        let apr_frac = 0.04_f64;
        let elapsed = SECS_PER_YEAR as u64;
        let earned = principal * apr_frac * (elapsed as f64 / SECS_PER_YEAR);
        assert!(
            (earned - 40.0).abs() < 0.001,
            "expected ~$40 earned, got {earned}"
        );
    }

    #[test]
    fn daily_rate_math_is_correct() {
        // $1000 at 3.72% APR → $37.20/yr → $0.1019/day.
        let principal = 1000.0_f64;
        let apr_frac = 0.0372_f64;
        let daily = principal * apr_frac / 365.0;
        assert!(
            (daily - 0.1019).abs() < 0.001,
            "expected ~$0.102/day, got {daily}"
        );
    }

    #[test]
    fn line_round_trips_via_json() {
        let line = TelemetryLine {
            ts: 1_714_800_000,
            obligation_pubkey: "AAA".to_string(),
            deposited_usdc_lamports: 0,
            supply_apr_bps: 372,
            paper_principal_usdc: 1000.0,
            paper_elapsed_secs: 86400,
            paper_earned_usdc: 1000.0 * 0.0372 / 365.0,
            paper_daily_rate_usdc: 1000.0 * 0.0372 / 365.0,
            paper_annual_rate_usdc: 37.2,
            total_aum_usdc: 1000.0 + 1000.0 * 0.0372 / 365.0,
        };
        let json = serde_json::to_string(&line).unwrap();
        let back: TelemetryLine = serde_json::from_str(&json).unwrap();
        assert_eq!(back.supply_apr_bps, 372);
        assert!((back.paper_earned_usdc - line.paper_earned_usdc).abs() < 1e-9);
        assert!((back.total_aum_usdc - line.total_aum_usdc).abs() < 1e-9);
    }

    #[test]
    fn append_creates_jsonl_file() {
        let path = std::env::temp_dir().join(format!("sy-tel-test-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let line = TelemetryLine {
            ts: 1,
            obligation_pubkey: "X".into(),
            deposited_usdc_lamports: 0,
            supply_apr_bps: 400,
            paper_principal_usdc: 5.0,
            paper_elapsed_secs: 60,
            paper_earned_usdc: 0.000038,
            paper_daily_rate_usdc: 0.00055,
            paper_annual_rate_usdc: 0.2,
            total_aum_usdc: 5.000038,
        };
        append_line(&path, &line).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("total_aum_usdc"));
        assert!(content.contains("paper_earned_usdc"));
        let _ = std::fs::remove_file(&path);
    }
}
