//! `multiply-daemon report` subcommand: read the pnl JSONL log, compute
//! trailing APR over a window, print a human-readable readout.

use anyhow::{Context, Result};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::pnl::PositionSnapshot;

pub fn report(log_path: &Path, since_secs: u64) -> Result<()> {
    let text = std::fs::read_to_string(log_path)
        .with_context(|| format!("read pnl log at {}", log_path.display()))?;

    let snaps: Vec<PositionSnapshot> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    if snaps.is_empty() {
        println!(
            "No snapshots in log {}. Run the daemon long enough for the beacon loop to write some.",
            log_path.display()
        );
        return Ok(());
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cutoff = now.saturating_sub(since_secs);
    let recent: Vec<&PositionSnapshot> = snaps
        .iter()
        .filter(|s| s.timestamp_unix >= cutoff)
        .collect();

    if recent.len() < 2 {
        println!(
            "Window {}s contains {} snapshot(s); need >=2 to compute APR. Wait longer.",
            since_secs,
            recent.len()
        );
        return Ok(());
    }

    let first = recent.first().unwrap();
    let last = recent.last().unwrap();

    let elapsed_secs = last
        .timestamp_unix
        .saturating_sub(first.timestamp_unix)
        .max(1);
    let pnl_uusdc: i64 = last.net_equity_uusdc - first.net_equity_uusdc;
    let initial_uusdc = first.net_equity_uusdc.max(1) as f64;
    let pnl_pct = pnl_uusdc as f64 / initial_uusdc;
    let seconds_per_year: f64 = 365.25 * 86400.0;
    let apr = pnl_pct * (seconds_per_year / elapsed_secs as f64);

    println!(
        "Multiply position report (window: {} s, {} snapshots)",
        elapsed_secs,
        recent.len()
    );
    println!(
        "  Initial net equity: ${:.6}",
        first.net_equity_uusdc as f64 / 1e6
    );
    println!(
        "  Current net equity: ${:.6}",
        last.net_equity_uusdc as f64 / 1e6
    );
    println!(
        "  Initial deposited:  ${:.6}",
        first.deposited_uusdc as f64 / 1e6
    );
    println!(
        "  Current deposited:  ${:.6}",
        last.deposited_uusdc as f64 / 1e6
    );
    println!(
        "  Initial borrowed:   ${:.6}",
        first.borrowed_uusdc as f64 / 1e6
    );
    println!(
        "  Current borrowed:   ${:.6}",
        last.borrowed_uusdc as f64 / 1e6
    );
    println!(
        "  PnL (window):       ${:+.6} ({:+.4}%)",
        pnl_uusdc as f64 / 1e6,
        pnl_pct * 100.0
    );
    println!("  Annualized APR:     {:+.2}%", apr * 100.0);

    Ok(())
}
