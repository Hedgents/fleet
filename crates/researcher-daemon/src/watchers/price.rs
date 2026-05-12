//! Pyth oracle price watcher. Reads PriceUpdateV2 accounts on a tick,
//! maintains a per-asset 1h ring buffer of (timestamp, price_micro_usd)
//! samples, and emits MarketSignal::PriceMovedBps when the windowed %
//! change crosses Notice or Important thresholds.
//!
//! Uses real Pyth decoding via `defi-protocols::protocols::pyth::decode_price`
//! (PriceUpdateV2 / Pull Oracle layout). Devnet feed lookup is not yet
//! supported in `pyth::feed_for_symbol`; callers pass feed pubkeys
//! explicitly via `--price-feed`. Mainnet sponsored addresses are
//! exposed via `pyth::feed_for_symbol(name, false)`.
//!
//! Buffer semantics:
//! - Each tick: prune samples older than 1h, then push current sample.
//! - Buffer hard-capped at 720 samples (prevents memory blowup if a
//!   misconfigured fast tick is left running).
//! - Lookback floor: emission requires ≥5min of in-buffer history. The
//!   first 5 minutes are warm-up — we record but do not emit.
//! - Comparison is current-vs-OLDEST in window (≥5min, ≤1h old).

use anyhow::{Context, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use crate::thresholds::{PRICE_1H_IMPORTANT_DELTA_BPS, PRICE_1H_NOTICE_DELTA_BPS};

/// 1h ring buffer window — samples older than this are dropped.
const RING_BUFFER_WINDOW: Duration = Duration::from_secs(3600);
/// Minimum in-buffer age before we'll emit a "1h move" signal. Prevents
/// spurious emissions during the first few minutes of operation.
const LOOKBACK_FLOOR: Duration = Duration::from_secs(300);
/// Hard cap on per-feed buffer length; bounds memory if poll_interval is
/// misconfigured to something tiny.
const MAX_SAMPLES_PER_FEED: usize = 720;

/// One Pyth price feed to watch.
#[derive(Clone)]
pub struct PriceFeedSpec {
    pub asset: AssetId,
    pub feed_pubkey: Pubkey,
    pub display_name: String,
}

#[derive(Clone, Copy)]
struct Sample {
    ts: Instant,
    price_micro_usd: i64,
}

/// Run the price watcher loop.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    rpc: Arc<RpcContext>,
    handle: NodeHandle,
    role: RoleIdentity,
    nonce: Arc<AtomicU64>,
    dedup: Arc<EmissionTracker>,
    feeds: Vec<PriceFeedSpec>,
    subscribers: Arc<tokio::sync::RwLock<Vec<[u8; 32]>>>,
    telemetry: Option<Arc<TelemetryHandle>>,
    poll_interval: Duration,
) -> Result<()> {
    let mut buffers: HashMap<Pubkey, VecDeque<Sample>> = HashMap::new();
    let mut tick = interval(poll_interval);

    info!(
        feed_count = feeds.len(),
        poll_interval_secs = poll_interval.as_secs(),
        "price watcher starting"
    );

    loop {
        tick.tick().await;
        for spec in &feeds {
            match poll_one(&rpc, spec).await {
                Ok(price_micro_usd) => {
                    let buf = buffers
                        .entry(spec.feed_pubkey)
                        .or_insert_with(VecDeque::new);
                    let now = Instant::now();

                    // Prune samples older than 1h.
                    while let Some(s) = buf.front() {
                        if now.duration_since(s.ts) > RING_BUFFER_WINDOW {
                            buf.pop_front();
                        } else {
                            break;
                        }
                    }
                    // Hard size cap.
                    while buf.len() >= MAX_SAMPLES_PER_FEED {
                        buf.pop_front();
                    }

                    // Compare to oldest in-window sample if we have ≥5min
                    // of history.
                    if let Some(oldest) = buf.front().copied() {
                        let age = now.duration_since(oldest.ts);
                        if age >= LOOKBACK_FLOOR {
                            let delta_bps =
                                compute_delta_bps(oldest.price_micro_usd, price_micro_usd);
                            if let Some(severity) = classify_delta(delta_bps) {
                                if dedup.should_emit(
                                    SignalKind::PriceMovedBps,
                                    spec.asset,
                                    severity,
                                ) {
                                    let payload = MarketSignal {
                                        kind: SignalKind::PriceMovedBps,
                                        asset: spec.asset,
                                        asset_mint: [0u8; 32],
                                        measurement_bps: delta_bps,
                                        severity,
                                        raised_at_unix: now_unix(),
                                        context_value: price_micro_usd.max(0) as u64,
                                    };
                                    let recipients = subscribers.read().await.clone();
                                    if recipients.is_empty() {
                                        info!(
                                            asset = %spec.display_name,
                                            delta_bps,
                                            "price signal generated but no subscribers — skipping broadcast"
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
                                            delta_bps,
                                            sent_count = sent,
                                            recipient_count = recipients.len(),
                                            "price signal broadcast"
                                        );
                                    }
                                }
                            }
                        }
                    }

                    debug!(
                        asset = %spec.display_name,
                        price_micro_usd,
                        buffer_len = buf.len(),
                        "price sample recorded"
                    );

                    buf.push_back(Sample {
                        ts: now,
                        price_micro_usd,
                    });
                }
                Err(e) => {
                    warn!(?e, asset = %spec.display_name, "pyth poll failed");
                }
            }
        }
    }
}

/// Poll a single Pyth PriceUpdateV2 account, return current price in
/// micro-USD (price * 10^-6, signed).
async fn poll_one(rpc: &RpcContext, spec: &PriceFeedSpec) -> Result<i64> {
    let data = rpc
        .client
        .get_account_data(&spec.feed_pubkey)
        .await
        .with_context(|| format!("get_account_data for Pyth feed {}", spec.feed_pubkey))?;
    let pp = pyth::decode_price(&data)
        .with_context(|| format!("decode Pyth PriceUpdateV2 at {}", spec.feed_pubkey))?;
    Ok(scale_to_micro_usd(pp.price, pp.expo))
}

/// Scale a Pyth (price, expo) pair to integer micro-USD (target expo = -6).
/// USD value = price × 10^expo. To express that in 10^-6 units, multiply
/// the integer by 10^(expo - target_expo) = 10^(expo + 6).
/// Saturating-multiplies in the "needs more digits" direction; integer-
/// divides (truncates toward zero) when the source has extra precision.
///
/// Made `pub` so other watchers (M6 stable_peg) can reuse it without
/// duplicating the scaling logic.
pub fn scale_to_micro_usd(price: i64, expo: i32) -> i64 {
    let target_expo: i32 = -6;
    let shift = expo - target_expo; // positive => multiply by 10^shift
    if shift >= 0 {
        let mul = 10i64.saturating_pow(shift as u32);
        price.saturating_mul(mul)
    } else {
        let div = 10i64.saturating_pow((-shift) as u32);
        if div == 0 {
            0
        } else {
            price / div
        }
    }
}

/// Returns (current - reference) / reference in basis points, signed.
/// 0 if reference is 0 (avoid div-by-zero panic).
fn compute_delta_bps(reference: i64, current: i64) -> i32 {
    if reference == 0 {
        return 0;
    }
    let delta = current as i128 - reference as i128;
    let bps = delta * 10_000_i128 / reference as i128;
    bps.clamp(i32::MIN as i128, i32::MAX as i128) as i32
}

/// Classify a signed bps delta into Notice / Important / None by absolute value.
fn classify_delta(delta_bps: i32) -> Option<SignalSeverity> {
    let abs = delta_bps.unsigned_abs() as i64;
    if abs >= PRICE_1H_IMPORTANT_DELTA_BPS as i64 {
        Some(SignalSeverity::Important)
    } else if abs >= PRICE_1H_NOTICE_DELTA_BPS as i64 {
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

/// Parse a `name:base58_pubkey:asset_enum` price-feed spec string.
/// `asset_enum`: SOL, ETH, BTC, USDC, USDT, JLP, Other.
pub fn parse_feed_spec(s: &str) -> Result<PriceFeedSpec> {
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    if parts.len() != 3 {
        anyhow::bail!("price feed spec must be `name:base58_pubkey:asset_enum`, got {s:?}");
    }
    let name = parts[0].to_string();
    let pubkey: Pubkey = parts[1]
        .parse()
        .with_context(|| format!("parsing price feed pubkey {:?}", parts[1]))?;
    let asset = parse_asset_id(parts[2])?;
    Ok(PriceFeedSpec {
        asset,
        feed_pubkey: pubkey,
        display_name: name,
    })
}

fn parse_asset_id(s: &str) -> Result<AssetId> {
    Ok(match s {
        "SOL" => AssetId::SOL,
        "ETH" => AssetId::ETH,
        "BTC" => AssetId::BTC,
        "USDC" => AssetId::USDC,
        "USDT" => AssetId::USDT,
        "JLP" => AssetId::JLP,
        "Other" => AssetId::Other,
        other => anyhow::bail!("unknown asset enum {other:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── scale_to_micro_usd ────────────────────────────────────────────
    #[test]
    fn scale_to_micro_at_neg_six_is_unchanged() {
        assert_eq!(scale_to_micro_usd(123_456_789, -6), 123_456_789);
    }

    #[test]
    fn scale_to_micro_at_neg_eight_divides_by_hundred() {
        // Pyth-typical: SOL feed has expo=-8. price=15_000_000_000 → $150 →
        // 150_000_000 micro-USD.
        assert_eq!(scale_to_micro_usd(15_000_000_000, -8), 150_000_000);
    }

    #[test]
    fn scale_to_micro_at_zero_multiplies() {
        // expo=0 means raw int is whole dollars. 1 USD → 1_000_000 micro.
        assert_eq!(scale_to_micro_usd(1, 0), 1_000_000);
    }

    #[test]
    fn scale_to_micro_at_neg_two_multiplies() {
        // expo=-2: cents. 100 cents = $1 = 1_000_000 micro.
        assert_eq!(scale_to_micro_usd(100, -2), 1_000_000);
    }

    // ── compute_delta_bps ─────────────────────────────────────────────
    #[test]
    fn compute_delta_bps_zero_change() {
        assert_eq!(compute_delta_bps(100_000_000, 100_000_000), 0);
    }

    #[test]
    fn compute_delta_bps_one_percent_up() {
        assert_eq!(compute_delta_bps(100_000_000, 101_000_000), 100);
    }

    #[test]
    fn compute_delta_bps_two_percent_down() {
        assert_eq!(compute_delta_bps(100_000_000, 98_000_000), -200);
    }

    #[test]
    fn compute_delta_bps_handles_zero_reference() {
        assert_eq!(compute_delta_bps(0, 100), 0);
    }

    #[test]
    fn compute_delta_bps_large_values_no_overflow() {
        // Big SOL prices (in micro-USD): 200_000_000 → 220_000_000 = +1000 bps.
        assert_eq!(compute_delta_bps(200_000_000, 220_000_000), 1000);
    }

    // ── classify_delta ────────────────────────────────────────────────
    #[test]
    fn classify_below_notice_returns_none() {
        let below = PRICE_1H_NOTICE_DELTA_BPS - 1;
        assert!(classify_delta(below).is_none());
        assert!(classify_delta(-below).is_none());
        assert!(classify_delta(0).is_none());
    }

    #[test]
    fn classify_at_notice_band() {
        assert_eq!(
            classify_delta(PRICE_1H_NOTICE_DELTA_BPS).unwrap() as u8,
            SignalSeverity::Notice as u8
        );
        assert_eq!(
            classify_delta(-PRICE_1H_NOTICE_DELTA_BPS).unwrap() as u8,
            SignalSeverity::Notice as u8
        );
    }

    #[test]
    fn classify_at_important_band() {
        assert_eq!(
            classify_delta(PRICE_1H_IMPORTANT_DELTA_BPS).unwrap() as u8,
            SignalSeverity::Important as u8
        );
        assert_eq!(
            classify_delta(-PRICE_1H_IMPORTANT_DELTA_BPS).unwrap() as u8,
            SignalSeverity::Important as u8
        );
    }

    #[test]
    fn classify_handles_i32_min_without_overflow() {
        // i32::MIN.abs() would overflow; classify_delta uses unsigned_abs.
        assert_eq!(
            classify_delta(i32::MIN).unwrap() as u8,
            SignalSeverity::Important as u8
        );
    }

    // ── parse_feed_spec ───────────────────────────────────────────────
    #[test]
    fn parse_feed_spec_round_trips() {
        let spec = parse_feed_spec("sol-usd:11111111111111111111111111111111:SOL").expect("parse");
        assert_eq!(spec.display_name, "sol-usd");
        assert_eq!(spec.asset as u16, AssetId::SOL as u16);
        assert_eq!(spec.feed_pubkey, Pubkey::default());
    }

    #[test]
    fn parse_feed_spec_rejects_bad_format() {
        assert!(parse_feed_spec("only:two").is_err());
        assert!(parse_feed_spec("name:notabase58:SOL").is_err());
        assert!(parse_feed_spec("name:11111111111111111111111111111111:WAT").is_err());
    }
}
