//! Periodic position telemetry. Polls own Kamino obligation, sums
//! deposit amounts, and appends a JSONL line per tick.
//!
//! Operator-facing observability — this is what we look at when the
//! $50 mainnet test is running. APR is a v0 placeholder (returns 0)
//! because Kamino's optimal-utilization curve is non-trivial to
//! reproduce in-process; operators can read APR off Kamino's UI for
//! now. TODO(post-M7): derive supply APR from reserve interest-rate
//! params (utilization × borrow_apr × (1 - protocol_take_rate)).
//!
//! Failure handling: every error path inside `poll_once` is non-fatal
//! and returns `Ok(())` after logging — telemetry must never take down
//! the daemon. The outer `run` loop logs at WARN if `poll_once`
//! itself bails (shouldn't happen) and continues ticking.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::interval;
use tracing::{debug, info, warn};

use zerox1_defi_runtime::rpc::RpcContext;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryLine {
    pub ts: u64,
    pub obligation_pubkey: String,
    /// Sum of `deposited_amount` across all deposit slots in the
    /// obligation. For stable-yield's single-reserve USDC operation
    /// this equals the cToken balance against the USDC reserve.
    /// (Stored as raw u64 — caller scales to USDC when reading.)
    pub deposited_usdc_lamports: u64,
    /// Supply APR estimate, basis points. v0 placeholder = 0.
    pub supply_apr_bps: u16,
}

pub async fn run(
    rpc: Arc<RpcContext>,
    payer: Pubkey,
    market: Pubkey,
    log_path: PathBuf,
    interval_secs: u64,
) -> Result<()> {
    info!(
        log_path = %log_path.display(),
        interval_secs,
        market = %market,
        "telemetry loop starting",
    );
    let mut tick = interval(Duration::from_secs(interval_secs.max(1)));
    // Prime the interval — first tick fires immediately, which we want
    // so the operator sees a line within seconds of boot.
    tick.tick().await;
    poll_and_log(&rpc, &payer, &market, &log_path).await;
    loop {
        tick.tick().await;
        poll_and_log(&rpc, &payer, &market, &log_path).await;
    }
}

async fn poll_and_log(
    rpc: &Arc<RpcContext>,
    payer: &Pubkey,
    market: &Pubkey,
    log_path: &Path,
) {
    if let Err(e) = poll_once(rpc, payer, market, log_path).await {
        warn!(?e, "telemetry poll failed");
    }
}

async fn poll_once(
    rpc: &Arc<RpcContext>,
    payer: &Pubkey,
    market: &Pubkey,
    log_path: &Path,
) -> Result<()> {
    use zerox1_defi_protocols::protocols::kamino::derive_user_obligation;
    use zerox1_defi_protocols::protocols::kamino_loader::fetch_obligation;

    let obligation_pk = derive_user_obligation(payer, market);

    let decoded_opt = fetch_obligation(&rpc.client, &obligation_pk)
        .await
        .context("fetch obligation for telemetry")?;

    let line = match decoded_opt {
        // No obligation account on chain yet (fresh wallet, no deposit
        // has ever landed). Emit a sentinel zero-line so the JSONL
        // shows the daemon is alive.
        None => TelemetryLine {
            ts: now_unix(),
            obligation_pubkey: obligation_pk.to_string(),
            deposited_usdc_lamports: 0,
            supply_apr_bps: 0,
        },
        Some(decoded) => {
            // For v0 we sum all deposits (assume single-reserve
            // operation). If stable-yield ever holds positions in
            // multiple reserves, we'll need to break this out per
            // reserve.
            let deposited: u64 = decoded
                .deposits
                .iter()
                .map(|d| d.deposited_amount)
                .fold(0u64, |acc, x| acc.saturating_add(x));

            let supply_apr_bps = compute_supply_apr_bps(rpc, market, &decoded)
                .await
                .unwrap_or_else(|e| {
                    warn!(?e, "APR estimate failed; writing 0");
                    0
                });

            TelemetryLine {
                ts: now_unix(),
                obligation_pubkey: obligation_pk.to_string(),
                deposited_usdc_lamports: deposited,
                supply_apr_bps,
            }
        }
    };

    append_line(log_path, &line)?;
    debug!(
        deposited_usdc_lamports = line.deposited_usdc_lamports,
        supply_apr_bps = line.supply_apr_bps,
        "telemetry tick recorded",
    );
    Ok(())
}

/// v0 placeholder. Real APR derivation is a follow-up:
///   supply_apr = utilization × borrow_apr × (1 - protocol_take_rate)
/// where Kamino's optimal-utilization curve makes borrow_apr
/// piecewise-linear in utilization. Operators can read APR off
/// Kamino's UI for the $50 mainnet test.
async fn compute_supply_apr_bps(
    _rpc: &Arc<RpcContext>,
    _market: &Pubkey,
    _decoded: &zerox1_defi_protocols::protocols::kamino_loader::DecodedObligation,
) -> Result<u16> {
    // TODO(post-M7): load Reserve account, read interest-rate params,
    // compute utilization from total_liquidity / (total_liquidity +
    // borrowed), evaluate the piecewise curve, derive supply APR.
    Ok(0)
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
    fn line_round_trips_via_json() {
        let line = TelemetryLine {
            ts: 1_714_800_000,
            obligation_pubkey: Pubkey::new_unique().to_string(),
            deposited_usdc_lamports: 50_000_000,
            supply_apr_bps: 425,
        };
        let json = serde_json::to_string(&line).unwrap();
        let back: TelemetryLine = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ts, 1_714_800_000);
        assert_eq!(back.deposited_usdc_lamports, 50_000_000);
        assert_eq!(back.supply_apr_bps, 425);
    }

    #[test]
    fn append_line_creates_and_appends() {
        // Use a unique path under the system temp dir; clean up at end.
        let unique = format!(
            "stable-yield-telemetry-test-{}-{}.jsonl",
            std::process::id(),
            now_unix()
        );
        let path = std::env::temp_dir().join(unique);
        let _ = std::fs::remove_file(&path);

        let line1 = TelemetryLine {
            ts: 100,
            obligation_pubkey: "AAA".to_string(),
            deposited_usdc_lamports: 1,
            supply_apr_bps: 0,
        };
        let line2 = TelemetryLine {
            ts: 200,
            obligation_pubkey: "BBB".to_string(),
            deposited_usdc_lamports: 2,
            supply_apr_bps: 0,
        };
        append_line(&path, &line1).unwrap();
        append_line(&path, &line2).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "two appends should yield two lines");
        assert!(lines[0].contains("\"ts\":100"));
        assert!(lines[1].contains("\"ts\":200"));
        let _ = std::fs::remove_file(&path);
    }
}
