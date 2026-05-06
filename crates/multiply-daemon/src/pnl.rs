//! Position telemetry — periodic snapshots + JSONL log.
//!
//! Each snapshot captures net-equity USD (deposited − borrowed) at a
//! given timestamp. The reporter subcommand reads the log to compute
//! trailing APR.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use zerox1_defi_runtime::rpc::RpcContext;

/// Kamino's scaled-fraction divisor — divide an sf value by 2^60 to get
/// the real USD value (per kamino_loader.rs comments).
const SF_DIVISOR_SHIFT: u32 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionSnapshot {
    pub timestamp_unix: u64,
    /// Total collateral USD × 1e6 (µUSD).
    pub deposited_uusdc: i64,
    /// Total debt USD × 1e6 (µUSD).
    pub borrowed_uusdc: i64,
    /// Net equity in µUSD (deposited - borrowed). Negative if underwater.
    pub net_equity_uusdc: i64,
}

/// Convert an sf-scaled u128 to µUSD (i64). The sf value is U=value/2^60 USD;
/// µUSD = U × 1e6.
fn sf_to_uusdc(sf: u128) -> i64 {
    // sf/2^60 USD × 1e6 µUSD/USD = sf × 1e6 / 2^60 µUSD.
    // To avoid precision loss in intermediate, shift first then multiply:
    //   (sf >> SF_DIVISOR_SHIFT) might lose mantissa bits, so we instead do
    //   (sf * 1_000_000) >> SF_DIVISOR_SHIFT.
    // u128 has plenty of headroom for sf × 1e6 (sf ≤ 2^64 typical, × 1e6 ≤ 2^84).
    let scaled = sf.saturating_mul(1_000_000);
    let uusdc = scaled >> SF_DIVISOR_SHIFT;
    uusdc.min(i64::MAX as u128) as i64
}

pub async fn snapshot(
    rpc: &Arc<RpcContext>,
    user: Pubkey,
    lending_market: Pubkey,
) -> Result<PositionSnapshot> {
    use zerox1_defi_protocols::protocols::kamino::derive_user_obligation;
    use zerox1_defi_protocols::protocols::kamino_loader::fetch_obligation;

    let obligation_addr = derive_user_obligation(&user, &lending_market);
    let decoded = fetch_obligation(&rpc.client, &obligation_addr)
        .await
        .context("fetch obligation for pnl snapshot")?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    match decoded {
        None => Ok(PositionSnapshot {
            timestamp_unix: now,
            deposited_uusdc: 0,
            borrowed_uusdc: 0,
            net_equity_uusdc: 0,
        }),
        Some(o) => {
            let dep = sf_to_uusdc(o.deposited_value_sf);
            let bor = sf_to_uusdc(o.borrowed_assets_market_value_sf);
            Ok(PositionSnapshot {
                timestamp_unix: now,
                deposited_uusdc: dep,
                borrowed_uusdc: bor,
                net_equity_uusdc: dep.saturating_sub(bor),
            })
        }
    }
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
        // 0 sf → 0 µUSD.
        assert_eq!(sf_to_uusdc(0), 0);

        // 1 sf → 1/2^60 USD = ~8.67e-19 USD = 0 µUSD (rounds to 0).
        assert_eq!(sf_to_uusdc(1), 0);

        // 2^60 sf → 1 USD = 1e6 µUSD.
        assert_eq!(sf_to_uusdc(1u128 << 60), 1_000_000);

        // 100 USD = 100 × 2^60 sf → 100e6 µUSD.
        let sf_100usd = 100u128 * (1u128 << 60);
        assert_eq!(sf_to_uusdc(sf_100usd), 100_000_000);
    }

    #[test]
    fn snapshot_round_trips_via_json() {
        let snap = PositionSnapshot {
            timestamp_unix: 1_714_800_000,
            deposited_uusdc: 50_000_000,    // $50
            borrowed_uusdc: 30_000_000,     // $30
            net_equity_uusdc: 20_000_000,   // $20
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: PositionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.deposited_uusdc, 50_000_000);
        assert_eq!(back.net_equity_uusdc, 20_000_000);
    }
}
