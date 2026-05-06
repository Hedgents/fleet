//! Drift perp funding rate watcher. Reads Drift PerpMarket accounts on
//! a tick, classifies current funding rate (in APR bps) against
//! thresholds, emits MarketSignal::PerpFundingAbove or
//! ::PerpFundingBelow when bands are crossed.
//!
//! Funding rate convention: positive = longs pay shorts (good for
//! short-perp basis trades); negative = shorts pay longs (basis trade
//! reversed). Watcher emits Above for positive crossings and Below
//! when funding flips negative or trends below threshold in the
//! negative direction.
//!
//! Funding-rate decode: M4 ships with a 0-stub. `defi-protocols/src/
//! protocols/` contains no Drift integration as of this milestone, and
//! the Drift PerpMarket layout (~1216 bytes, AnchorSerialize) carries
//! ~150 fields including AMM curve params, oracle config, insurance
//! fund pointers, etc. Real funding extraction requires either
//! pulling drift-program as a dep (heavy) or reverse-engineering the
//! `last_funding_rate` offset against drift's IDL — a follow-up.
//!
//! For M4 we ship the watcher loop (poll cadence, sign-flip
//! detection, threshold classification, dedup, signal broadcast) and
//! stub the funding rate to 0 — proving the emission infrastructure
//! works end-to-end. Mirror of M3's lending_rate stub pattern.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use solana_sdk::pubkey::Pubkey;
use tokio::time::interval;
use tracing::{info, warn};

use zerox1_defi_runtime::{identity::RoleIdentity, rpc::RpcContext};
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::fleet::researcher::{AssetId, MarketSignal, SignalKind, SignalSeverity};

use crate::dedup::EmissionTracker;
use crate::signal;
use crate::telemetry::TelemetryHandle;
use crate::thresholds::{
    FUNDING_RATE_IMPORTANT_THRESHOLD_BPS, FUNDING_RATE_INFO_THRESHOLD_BPS,
    FUNDING_RATE_NOTICE_THRESHOLD_BPS,
};

/// One Drift perp market to watch.
#[derive(Clone)]
pub struct PerpMarketSpec {
    pub asset: AssetId,
    pub market_pubkey: Pubkey,
    pub display_name: String,
}

#[derive(Clone, Copy, Default)]
struct LastKnown {
    #[allow(dead_code)]
    funding_apr_bps: i32,
    sign_was_positive: bool,
}

/// Run the perp funding watcher loop. `subscribers` are the recipients
/// to broadcast signals to (shared across all watchers).
#[allow(clippy::too_many_arguments)]
pub async fn run(
    rpc: Arc<RpcContext>,
    handle: NodeHandle,
    role: RoleIdentity,
    nonce: Arc<AtomicU64>,
    dedup: Arc<EmissionTracker>,
    markets: Vec<PerpMarketSpec>,
    subscribers: Arc<tokio::sync::RwLock<Vec<[u8; 32]>>>,
    telemetry: Option<Arc<TelemetryHandle>>,
    poll_interval: Duration,
) -> Result<()> {
    let mut last_known: HashMap<Pubkey, LastKnown> = HashMap::new();
    let mut tick = interval(poll_interval);

    info!(
        market_count = markets.len(),
        poll_interval_secs = poll_interval.as_secs(),
        "perp_funding watcher starting"
    );

    loop {
        tick.tick().await;
        for spec in &markets {
            match poll_one(&rpc, spec).await {
                Ok(funding_apr_bps) => {
                    let prev = last_known.get(&spec.market_pubkey).copied();

                    // First-observation guard: seed and skip (M3 pattern).
                    if prev.is_none() {
                        last_known.insert(
                            spec.market_pubkey,
                            LastKnown {
                                funding_apr_bps,
                                sign_was_positive: funding_apr_bps >= 0,
                            },
                        );
                        info!(
                            market = %spec.display_name,
                            funding_apr_bps,
                            "perp_funding first observation seeded"
                        );
                        continue;
                    }
                    let prev = prev.unwrap();

                    // Classify CURRENT positive level (not delta) — positive
                    // funding is "longs paying shorts at rate X". Higher = more
                    // interesting for basis trades.
                    if funding_apr_bps > 0 {
                        if let Some(severity) = classify_above(funding_apr_bps) {
                            if dedup.should_emit(
                                SignalKind::PerpFundingAbove,
                                spec.asset,
                                severity,
                            ) {
                                broadcast_signal(
                                    &handle,
                                    &role,
                                    &nonce,
                                    &subscribers,
                                    telemetry.as_ref(),
                                    SignalKind::PerpFundingAbove,
                                    spec.asset,
                                    funding_apr_bps,
                                    severity,
                                )
                                .await;
                            }
                        }
                    }

                    // Sign-flip detection: if funding flipped sign, that's
                    // always a Notice (even if below the absolute Above/Below
                    // threshold band). Basis-trade entry/exit cue.
                    let now_positive = funding_apr_bps >= 0;
                    if now_positive != prev.sign_was_positive {
                        let kind = if now_positive {
                            SignalKind::PerpFundingAbove
                        } else {
                            SignalKind::PerpFundingBelow
                        };
                        if dedup.should_emit(kind, spec.asset, SignalSeverity::Notice) {
                            broadcast_signal(
                                &handle,
                                &role,
                                &nonce,
                                &subscribers,
                                telemetry.as_ref(),
                                kind,
                                spec.asset,
                                funding_apr_bps,
                                SignalSeverity::Notice,
                            )
                            .await;
                        }
                    }

                    // Negative funding past threshold: PerpFundingBelow.
                    if funding_apr_bps < 0 {
                        if let Some(severity) = classify_below(funding_apr_bps) {
                            if dedup.should_emit(
                                SignalKind::PerpFundingBelow,
                                spec.asset,
                                severity,
                            ) {
                                broadcast_signal(
                                    &handle,
                                    &role,
                                    &nonce,
                                    &subscribers,
                                    telemetry.as_ref(),
                                    SignalKind::PerpFundingBelow,
                                    spec.asset,
                                    funding_apr_bps,
                                    severity,
                                )
                                .await;
                            }
                        }
                    }

                    last_known.insert(
                        spec.market_pubkey,
                        LastKnown {
                            funding_apr_bps,
                            sign_was_positive: now_positive,
                        },
                    );
                }
                Err(e) => {
                    // Non-fatal — log and skip this market this tick.
                    warn!(?e, market = %spec.display_name, "perp_funding poll failed");
                }
            }
        }
    }
}

/// Poll a single Drift PerpMarket. Returns funding rate in APR bps
/// (positive = longs pay shorts, negative = shorts pay longs).
///
/// M4 v0: fetches the raw PerpMarket account to verify it exists, then
/// returns 0. Real funding extraction (decoding `last_funding_rate` from
/// the AnchorSerialize layout) is a follow-up — the watcher loop,
/// classification, and emission pipeline all work either way.
async fn poll_one(rpc: &RpcContext, spec: &PerpMarketSpec) -> Result<i32> {
    let _data = rpc
        .client
        .get_account_data(&spec.market_pubkey)
        .await
        .with_context(|| format!("get_account_data for Drift PerpMarket {}", spec.market_pubkey))?;

    // TODO(M4-polish): decode Drift PerpMarket layout. The relevant
    // field is `amm.last_funding_rate` (i64, in funding-rate-precision
    // units = 1e9). Annualize: funding_rate / 1e9 × hours_in_year /
    // funding_period_hours × 10_000 = APR bps. Drift funding period
    // is 1h (3600s). Offset to be reverse-engineered against the IDL
    // at https://github.com/drift-labs/protocol-v2.
    Ok(0)
}

/// Classify a positive funding rate (APR bps) into a severity band.
/// Returns None if below the Info threshold.
fn classify_above(apr_bps: i32) -> Option<SignalSeverity> {
    if apr_bps >= FUNDING_RATE_IMPORTANT_THRESHOLD_BPS {
        Some(SignalSeverity::Important)
    } else if apr_bps >= FUNDING_RATE_NOTICE_THRESHOLD_BPS {
        Some(SignalSeverity::Notice)
    } else if apr_bps >= FUNDING_RATE_INFO_THRESHOLD_BPS {
        Some(SignalSeverity::Info)
    } else {
        None
    }
}

/// Classify a negative funding rate (APR bps) into a severity band by
/// absolute magnitude. Returns None if `|apr_bps|` is below the Info
/// threshold.
fn classify_below(apr_bps: i32) -> Option<SignalSeverity> {
    let abs = apr_bps.abs();
    if abs >= FUNDING_RATE_IMPORTANT_THRESHOLD_BPS {
        Some(SignalSeverity::Important)
    } else if abs >= FUNDING_RATE_NOTICE_THRESHOLD_BPS {
        Some(SignalSeverity::Notice)
    } else if abs >= FUNDING_RATE_INFO_THRESHOLD_BPS {
        Some(SignalSeverity::Info)
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
async fn broadcast_signal(
    handle: &NodeHandle,
    role: &RoleIdentity,
    nonce: &Arc<AtomicU64>,
    subscribers: &Arc<tokio::sync::RwLock<Vec<[u8; 32]>>>,
    telemetry: Option<&Arc<TelemetryHandle>>,
    kind: SignalKind,
    asset: AssetId,
    measurement_bps: i32,
    severity: SignalSeverity,
) {
    let payload = MarketSignal {
        kind,
        asset,
        asset_mint: [0u8; 32],
        measurement_bps,
        severity,
        raised_at_unix: now_unix(),
        context_value: 0,
    };
    let recipients = subscribers.read().await.clone();
    if recipients.is_empty() {
        info!(
            ?kind,
            ?asset,
            "funding signal generated but no subscribers — skipping broadcast"
        );
        return;
    }
    let sent = signal::emit_broadcast(handle, role, nonce, &recipients, payload, telemetry).await;
    info!(
        sent_count = sent,
        recipient_count = recipients.len(),
        "funding signal broadcast"
    );
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse a `name:base58_pubkey:asset_enum` perp-market spec string.
/// `asset_enum` is one of: `SOL`, `ETH`, `BTC`, `USDC`, `USDT`, `JLP`,
/// `Other` (case-sensitive).
pub fn parse_market_spec(s: &str) -> Result<PerpMarketSpec> {
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    if parts.len() != 3 {
        anyhow::bail!(
            "perp market spec must be `name:base58_pubkey:asset_enum`, got {s:?}"
        );
    }
    let name = parts[0].to_string();
    let pubkey: Pubkey = parts[1]
        .parse()
        .with_context(|| format!("parsing perp market pubkey {:?}", parts[1]))?;
    let asset = parse_asset_id(parts[2])?;
    Ok(PerpMarketSpec {
        asset,
        market_pubkey: pubkey,
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

    #[test]
    fn classify_above_below_info_returns_none() {
        assert!(classify_above(400).is_none());
        assert!(classify_above(0).is_none());
    }

    #[test]
    fn classify_above_at_info_band() {
        assert_eq!(
            classify_above(500).unwrap() as u8,
            SignalSeverity::Info as u8
        );
        assert_eq!(
            classify_above(1999).unwrap() as u8,
            SignalSeverity::Info as u8
        );
    }

    #[test]
    fn classify_above_at_notice_band() {
        assert_eq!(
            classify_above(2000).unwrap() as u8,
            SignalSeverity::Notice as u8
        );
        assert_eq!(
            classify_above(4999).unwrap() as u8,
            SignalSeverity::Notice as u8
        );
    }

    #[test]
    fn classify_above_at_important_band() {
        assert_eq!(
            classify_above(5000).unwrap() as u8,
            SignalSeverity::Important as u8
        );
        assert_eq!(
            classify_above(50_000).unwrap() as u8,
            SignalSeverity::Important as u8
        );
    }

    #[test]
    fn classify_below_uses_abs_value() {
        assert_eq!(
            classify_below(-2500).unwrap() as u8,
            SignalSeverity::Notice as u8
        );
        assert_eq!(
            classify_below(-500).unwrap() as u8,
            SignalSeverity::Info as u8
        );
        assert_eq!(
            classify_below(-5000).unwrap() as u8,
            SignalSeverity::Important as u8
        );
        assert!(classify_below(-100).is_none());
    }

    #[test]
    fn classify_above_negative_returns_none() {
        // classify_above is for positive values; negative is classify_below's job.
        assert!(classify_above(-2500).is_none());
        assert!(classify_above(-5000).is_none());
    }

    #[test]
    fn parse_market_spec_round_trips() {
        let spec = parse_market_spec("sol-perp:11111111111111111111111111111111:SOL")
            .expect("parse");
        assert_eq!(spec.display_name, "sol-perp");
        assert_eq!(spec.asset as u16, AssetId::SOL as u16);
        assert_eq!(spec.market_pubkey, Pubkey::default());
    }

    #[test]
    fn parse_market_spec_rejects_bad_format() {
        assert!(parse_market_spec("only:two").is_err());
        assert!(parse_market_spec("name:notabase58:SOL").is_err());
        assert!(parse_market_spec("name:11111111111111111111111111111111:WAT").is_err());
    }
}
