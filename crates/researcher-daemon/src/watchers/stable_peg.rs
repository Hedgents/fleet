//! Stablecoin peg watcher. Reads USDC/USD and USDT/USD Pyth feeds on
//! a tick, computes deviation from $1.00 in basis points, emits
//! MarketSignal::StableDepegBps when |deviation| crosses Notice (30bps)
//! or Important (100bps) bands.
//!
//! Differences from the M5 price watcher:
//! - **No ring buffer.** Depeg is a current-state check, not a
//!   delta-over-time. Each tick recomputes deviation from the peg.
//! - **Symmetric thresholds.** Above-peg ($1.005) and below-peg ($0.995)
//!   are both depeg events of equal severity.
//! - **Asset-specific signals.** Each feed is bound to an `AssetId`
//!   (USDC or USDT) so the dedup tracker scopes per-asset.
//!
//! Important depeg = fleet-wide pause signal. Consumer daemons
//! (multiply, stable-yield, hedgedjlp, speculator) should react by
//! pausing position opens and considering withdrawals — but the
//! reaction lives in their own dispatch logic; researcher only signals.

use anyhow::{Context, Result};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use solana_sdk::pubkey::Pubkey;
use tokio::time::interval;
use tracing::{debug, info, warn};

use zerox1_defi_protocols::protocols::pyth;
use zerox1_defi_runtime::{identity::RoleIdentity, rpc::RpcContext};
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::fleet::researcher::{AssetId, MarketSignal, SignalKind, SignalSeverity};

use crate::dedup::EmissionTracker;
use crate::signal;
use crate::telemetry::TelemetryHandle;
use crate::thresholds::{STABLE_DEPEG_IMPORTANT_BPS, STABLE_DEPEG_NOTICE_BPS};
use crate::watchers::price::scale_to_micro_usd;

/// $1.00 in micro-USD. The peg target.
const PEG_MICRO_USD: i64 = 1_000_000;

/// One stablecoin Pyth feed to watch.
#[derive(Clone)]
pub struct StableFeedSpec {
    /// Asset enum — must be USDC or USDT for sensible signal routing,
    /// but we don't enforce that in code (callers are trusted to pass
    /// the right enum).
    pub asset: AssetId,
    pub feed_pubkey: Pubkey,
    pub display_name: String,
}

/// Run the stable-peg watcher loop. Polls every `poll_interval`, emits
/// `MarketSignal::StableDepegBps` when |deviation| ≥ Notice band.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    rpc: Arc<RpcContext>,
    handle: NodeHandle,
    role: RoleIdentity,
    nonce: Arc<AtomicU64>,
    dedup: Arc<EmissionTracker>,
    feeds: Vec<StableFeedSpec>,
    subscribers: Arc<tokio::sync::RwLock<Vec<[u8; 32]>>>,
    telemetry: Option<Arc<TelemetryHandle>>,
    poll_interval: Duration,
) -> Result<()> {
    let mut tick = interval(poll_interval);
    info!(
        feed_count = feeds.len(),
        poll_interval_secs = poll_interval.as_secs(),
        "stable_peg watcher starting"
    );

    loop {
        tick.tick().await;
        for spec in &feeds {
            match poll_one(&rpc, spec).await {
                Ok(price_micro_usd) => {
                    let deviation_bps = compute_deviation_bps(price_micro_usd);
                    if let Some(severity) = classify_deviation(deviation_bps) {
                        if dedup.should_emit(
                            SignalKind::StableDepegBps,
                            spec.asset,
                            severity,
                        ) {
                            let payload = MarketSignal {
                                kind: SignalKind::StableDepegBps,
                                asset: spec.asset,
                                asset_mint: [0u8; 32],
                                measurement_bps: deviation_bps,
                                severity,
                                raised_at_unix: now_unix(),
                                context_value: price_micro_usd.max(0) as u64,
                            };
                            let recipients = subscribers.read().await.clone();
                            if recipients.is_empty() {
                                info!(
                                    asset = %spec.display_name,
                                    deviation_bps,
                                    severity = ?severity,
                                    "stable_peg signal generated but no subscribers — skipping broadcast"
                                );
                            } else {
                                let sent = signal::emit_broadcast(
                                    &handle,
                                    &role,
                                    &nonce,
                                    &recipients,
                                    payload,
                                    telemetry.as_ref(),
                                )
                                .await;
                                info!(
                                    asset = %spec.display_name,
                                    deviation_bps,
                                    severity = ?severity,
                                    sent_count = sent,
                                    recipient_count = recipients.len(),
                                    "stable_peg signal broadcast"
                                );
                            }
                        }
                    } else {
                        // On-peg: nothing to emit. Debug-log so operators
                        // tailing logs can confirm the watcher is alive.
                        debug!(
                            asset = %spec.display_name,
                            deviation_bps,
                            price_micro_usd,
                            "stable on-peg"
                        );
                    }
                }
                Err(e) => warn!(?e, asset = %spec.display_name, "stable_peg poll failed"),
            }
        }
    }
}

/// Read the Pyth feed once, return current price in micro-USD.
async fn poll_one(rpc: &RpcContext, spec: &StableFeedSpec) -> Result<i64> {
    let data = rpc
        .client
        .get_account_data(&spec.feed_pubkey)
        .await
        .with_context(|| {
            format!("get_account_data for stable Pyth feed {}", spec.feed_pubkey)
        })?;
    let pp = pyth::decode_price(&data)
        .with_context(|| format!("decode Pyth PriceUpdateV2 at {}", spec.feed_pubkey))?;
    Ok(scale_to_micro_usd(pp.price, pp.expo))
}

/// Compute signed deviation from peg in basis points.
/// `(price - 1_000_000) * 10_000 / 1_000_000`. Positive = above peg.
/// Uses i128 intermediate to avoid overflow on extreme prices.
fn compute_deviation_bps(price_micro_usd: i64) -> i32 {
    let delta = price_micro_usd as i128 - PEG_MICRO_USD as i128;
    let bps = delta * 10_000_i128 / PEG_MICRO_USD as i128;
    bps.clamp(i32::MIN as i128, i32::MAX as i128) as i32
}

/// Classify |deviation_bps| into Notice / Important / None bands.
/// Uses `unsigned_abs` to safely handle `i32::MIN` (where `.abs()` would
/// overflow).
fn classify_deviation(bps: i32) -> Option<SignalSeverity> {
    let abs = bps.unsigned_abs();
    if abs >= STABLE_DEPEG_IMPORTANT_BPS as u32 {
        Some(SignalSeverity::Important)
    } else if abs >= STABLE_DEPEG_NOTICE_BPS as u32 {
        Some(SignalSeverity::Notice)
    } else {
        None
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse a `name:base58_pubkey:asset_enum` stable-feed spec string.
/// `asset_enum`: USDC or USDT. Other variants are rejected — peg watching
/// only makes sense for stablecoins.
pub fn parse_feed_spec(s: &str) -> Result<StableFeedSpec> {
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    if parts.len() != 3 {
        anyhow::bail!(
            "stable feed spec must be `name:base58_pubkey:asset_enum`, got {s:?}"
        );
    }
    let name = parts[0].to_string();
    let pubkey: Pubkey = parts[1]
        .parse()
        .with_context(|| format!("parsing stable feed pubkey {:?}", parts[1]))?;
    let asset = parse_stable_asset_id(parts[2])?;
    Ok(StableFeedSpec {
        asset,
        feed_pubkey: pubkey,
        display_name: name,
    })
}

fn parse_stable_asset_id(s: &str) -> Result<AssetId> {
    Ok(match s {
        "USDC" => AssetId::USDC,
        "USDT" => AssetId::USDT,
        other => anyhow::bail!(
            "stable peg watcher only supports USDC or USDT, got {other:?}"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── compute_deviation_bps ─────────────────────────────────────────
    #[test]
    fn at_peg_returns_zero() {
        assert_eq!(compute_deviation_bps(1_000_000), 0);
    }

    #[test]
    fn above_peg_50bps() {
        // $1.005 = 1_005_000 micro-USD → +50 bps.
        assert_eq!(compute_deviation_bps(1_005_000), 50);
    }

    #[test]
    fn below_peg_30bps() {
        // $0.997 = 997_000 micro-USD → -30 bps.
        assert_eq!(compute_deviation_bps(997_000), -30);
    }

    #[test]
    fn one_percent_above() {
        // $1.01 = 1_010_000 → +100 bps.
        assert_eq!(compute_deviation_bps(1_010_000), 100);
    }

    #[test]
    fn one_percent_below() {
        // $0.99 = 990_000 → -100 bps.
        assert_eq!(compute_deviation_bps(990_000), -100);
    }

    #[test]
    fn extreme_collapse() {
        // $0.50 = 500_000 → -5000 bps.
        assert_eq!(compute_deviation_bps(500_000), -5000);
    }

    // ── classify_deviation ────────────────────────────────────────────
    #[test]
    fn classify_at_peg_returns_none() {
        assert!(classify_deviation(0).is_none());
        assert!(classify_deviation(20).is_none());
        assert!(classify_deviation(-29).is_none());
    }

    #[test]
    fn classify_at_notice() {
        assert_eq!(
            classify_deviation(STABLE_DEPEG_NOTICE_BPS).unwrap() as u8,
            SignalSeverity::Notice as u8
        );
        assert_eq!(
            classify_deviation(-STABLE_DEPEG_NOTICE_BPS).unwrap() as u8,
            SignalSeverity::Notice as u8
        );
    }

    #[test]
    fn classify_at_important() {
        assert_eq!(
            classify_deviation(STABLE_DEPEG_IMPORTANT_BPS).unwrap() as u8,
            SignalSeverity::Important as u8
        );
        assert_eq!(
            classify_deviation(-150).unwrap() as u8,
            SignalSeverity::Important as u8
        );
    }

    #[test]
    fn classify_handles_imin() {
        // i32::MIN.unsigned_abs() must not panic; absolute value far
        // exceeds Important threshold so we expect Important.
        assert_eq!(
            classify_deviation(i32::MIN).unwrap() as u8,
            SignalSeverity::Important as u8
        );
    }

    // ── parse_feed_spec ───────────────────────────────────────────────
    #[test]
    fn parse_feed_spec_round_trips_usdc() {
        let spec =
            parse_feed_spec("usdc:11111111111111111111111111111111:USDC").expect("parse");
        assert_eq!(spec.display_name, "usdc");
        assert_eq!(spec.asset as u16, AssetId::USDC as u16);
        assert_eq!(spec.feed_pubkey, Pubkey::default());
    }

    #[test]
    fn parse_feed_spec_round_trips_usdt() {
        let spec =
            parse_feed_spec("usdt:11111111111111111111111111111111:USDT").expect("parse");
        assert_eq!(spec.asset as u16, AssetId::USDT as u16);
    }

    #[test]
    fn parse_feed_spec_rejects_non_stable() {
        // SOL is not a stablecoin — peg watching shouldn't accept it.
        assert!(parse_feed_spec("sol:11111111111111111111111111111111:SOL").is_err());
    }

    #[test]
    fn parse_feed_spec_rejects_bad_format() {
        assert!(parse_feed_spec("only:two").is_err());
        assert!(parse_feed_spec("name:notabase58:USDC").is_err());
    }
}
