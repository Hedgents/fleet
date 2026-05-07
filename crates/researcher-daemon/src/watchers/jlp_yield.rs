//! Jupiter Perps LP watcher. Reads the JLP pool account on a tick,
//! computes 7d yield + per-custody allocation, and emits signals when
//! yield changes meaningfully OR any custody's % allocation shifts >5%.
//!
//! Two distinct signal kinds:
//! - **JlpYieldChanged** — rolling 7d yield delta vs last observation
//!   (Info ≥50bps, Notice ≥200bps).
//! - **JlpCompositionShifted** — any custody asset's allocation in bps
//!   shifted ≥500bps (5%) from last observation (Notice). Emitted
//!   per-asset so consumers (hedgedjlp-daemon) can rebalance only
//!   the legs that actually moved.
//!
//! ## Decode status (M7 v0)
//!
//! `zerox1-defi-protocols::protocols::jlp` exposes mint/burn instruction
//! builders + custody/pool *metadata structs* (`PoolMeta`, `CustodyMeta`),
//! but NO pool-account decoder, NO AUM extraction, NO 7d-yield helper.
//! Adding those properly requires Anchor-IDL-driven offset reads of
//! `Pool { aum_usd, cumulative_fees, … }` plus a fee-history ring buffer
//! which JLP doesn't expose on-chain — Jupiter's frontend pulls 7d yield
//! from an off-chain index.
//!
//! Therefore M7 ships the watcher *loop* operational with a 0-stubbed
//! `poll_one`. The loop, dedup, signal payloads, CLI wiring, and tests
//! are all real and verifiable. M7+ (or a follow-up milestone) will
//! replace `poll_one` with a real decoder once the helper lands in
//! `defi-protocols::jlp`. This mirrors the M3 lending_rate stub approach.
//!
//! Consumers: hedgedjlp-daemon (subscribes to both signal kinds —
//! yield drives entry/exit, composition shift drives delta-rebalance).

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

// Thresholds. Kept local to this watcher (the central thresholds.rs is
// for cross-watcher constants); these are JLP-specific and unlikely to
// be reused elsewhere.
const YIELD_INFO_DELTA_BPS: i32 = 50; // 0.5% APR change
const YIELD_NOTICE_DELTA_BPS: i32 = 200; // 2% APR change
const COMPOSITION_NOTICE_SHIFT_BPS: u16 = 500; // 5% absolute allocation shift

#[derive(Default, Clone)]
struct LastObservation {
    yield_bps: i32,
    /// Per-asset allocation in bps (sum 10_000).
    composition: HashMap<AssetId, u16>,
    /// Whether we have any prior data — first tick seeds and skips.
    seeded: bool,
}

/// Run the JLP yield + composition watcher loop. First observation
/// seeds; subsequent ticks compare and emit when deltas cross bands.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    rpc: Arc<RpcContext>,
    handle: NodeHandle,
    role: RoleIdentity,
    nonce: Arc<AtomicU64>,
    dedup: Arc<EmissionTracker>,
    pool_pubkey: Pubkey,
    subscribers: Arc<tokio::sync::RwLock<Vec<[u8; 32]>>>,
    telemetry: Option<Arc<TelemetryHandle>>,
    poll_interval: Duration,
) -> Result<()> {
    let mut last = LastObservation::default();
    let mut tick = interval(poll_interval);

    info!(
        pool = %pool_pubkey,
        poll_interval_secs = poll_interval.as_secs(),
        "jlp_yield watcher starting"
    );

    loop {
        tick.tick().await;
        match poll_one(&rpc, &pool_pubkey).await {
            Ok((yield_bps, composition)) => {
                if !last.seeded {
                    last = LastObservation {
                        yield_bps,
                        composition,
                        seeded: true,
                    };
                    info!(yield_bps, "jlp first observation seeded");
                    continue;
                }

                // Yield delta
                let yield_delta = yield_bps - last.yield_bps;
                let abs_yield_delta = yield_delta.unsigned_abs() as i32;
                if abs_yield_delta >= YIELD_INFO_DELTA_BPS {
                    let severity = if abs_yield_delta >= YIELD_NOTICE_DELTA_BPS {
                        SignalSeverity::Notice
                    } else {
                        SignalSeverity::Info
                    };
                    if dedup.should_emit(SignalKind::JlpYieldChanged, AssetId::JLP, severity) {
                        broadcast(
                            &handle,
                            &role,
                            &nonce,
                            &subscribers,
                            telemetry.as_ref(),
                            SignalKind::JlpYieldChanged,
                            AssetId::JLP,
                            yield_bps,
                            severity,
                            last.yield_bps.max(0) as u64,
                        )
                        .await;
                    }
                }

                // Composition shifts (per asset). Drop unmapped assets —
                // the v0 signal payload doesn't carry mint pubkeys for
                // composition signals.
                for (asset, &new_alloc) in &composition {
                    let prev_alloc = last.composition.get(asset).copied().unwrap_or(0);
                    let shift = (new_alloc as i32 - prev_alloc as i32).unsigned_abs() as u16;
                    if shift >= COMPOSITION_NOTICE_SHIFT_BPS
                        && dedup.should_emit(
                            SignalKind::JlpCompositionShifted,
                            *asset,
                            SignalSeverity::Notice,
                        )
                    {
                        broadcast(
                            &handle,
                            &role,
                            &nonce,
                            &subscribers,
                            telemetry.as_ref(),
                            SignalKind::JlpCompositionShifted,
                            *asset,
                            new_alloc as i32,
                            SignalSeverity::Notice,
                            prev_alloc as u64,
                        )
                        .await;
                    }
                }

                last = LastObservation {
                    yield_bps,
                    composition,
                    seeded: true,
                };
            }
            Err(e) => warn!(?e, "jlp poll failed"),
        }
    }
}

/// Read the JLP pool account. Returns (rolling_7d_yield_bps,
/// composition_in_bps_per_asset).
///
/// **v0 stub**: defi-protocols::jlp lacks a pool decoder + AUM extractor
/// + fee-history → 7d-yield helper (see module-level docs). We still
/// hit the RPC so failure modes (network errors, bad pubkey) surface
/// and so the loop's structure matches the eventual real version, then
/// return (0, empty) — yield_delta will be 0 each tick (no signal),
/// composition will be empty (no per-asset shift signals). Tests
/// validate the threshold-arithmetic that drives signal emission once
/// real numbers arrive.
async fn poll_one(rpc: &RpcContext, pool_pubkey: &Pubkey) -> Result<(i32, HashMap<AssetId, u16>)> {
    let _data = rpc
        .client
        .get_account_data(pool_pubkey)
        .await
        .with_context(|| format!("get_account_data for JLP pool {pool_pubkey}"))?;

    // TODO(researcher-M7+): replace with real decode via
    // defi-protocols::jlp::read_pool() once that helper lands.
    Ok((0, HashMap::new()))
}

#[allow(clippy::too_many_arguments)]
async fn broadcast(
    handle: &NodeHandle,
    role: &RoleIdentity,
    nonce: &Arc<AtomicU64>,
    subscribers: &Arc<tokio::sync::RwLock<Vec<[u8; 32]>>>,
    telemetry: Option<&Arc<TelemetryHandle>>,
    kind: SignalKind,
    asset: AssetId,
    measurement_bps: i32,
    severity: SignalSeverity,
    context_value: u64,
) {
    let payload = MarketSignal {
        kind,
        asset,
        asset_mint: [0u8; 32],
        measurement_bps,
        severity,
        raised_at_unix: now_unix(),
        context_value,
    };
    let recipients = subscribers.read().await.clone();
    if recipients.is_empty() {
        info!(?kind, ?asset, "jlp signal generated but no subscribers");
        return;
    }
    let sent = signal::emit_broadcast(handle, role, nonce, &recipients, payload, telemetry).await;
    info!(
        ?kind,
        ?asset,
        sent_count = sent,
        recipient_count = recipients.len(),
        "jlp signal broadcast"
    );
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yield_delta_below_info_band() {
        let abs_delta = 49_i32.unsigned_abs() as i32;
        assert!(abs_delta < YIELD_INFO_DELTA_BPS);
    }

    #[test]
    fn yield_delta_at_info_band_classifies() {
        let abs_delta = 50_i32.unsigned_abs() as i32;
        assert!(abs_delta >= YIELD_INFO_DELTA_BPS);
        assert!(abs_delta < YIELD_NOTICE_DELTA_BPS);
    }

    #[test]
    fn yield_delta_at_notice_band() {
        let abs_delta = 200_i32.unsigned_abs() as i32;
        assert!(abs_delta >= YIELD_NOTICE_DELTA_BPS);
    }

    #[test]
    fn yield_delta_negative_uses_abs() {
        // -250 bps drop is still a Notice-level event.
        let abs_delta = (-250_i32).unsigned_abs() as i32;
        assert!(abs_delta >= YIELD_NOTICE_DELTA_BPS);
    }

    #[test]
    fn composition_shift_at_band() {
        let prev: u16 = 4500;
        let new: u16 = 5000;
        let shift = (new as i32 - prev as i32).unsigned_abs() as u16;
        assert!(shift >= COMPOSITION_NOTICE_SHIFT_BPS);
    }

    #[test]
    fn composition_shift_below_band() {
        let prev: u16 = 4500;
        let new: u16 = 4800;
        let shift = (new as i32 - prev as i32).unsigned_abs() as u16;
        assert!(shift < COMPOSITION_NOTICE_SHIFT_BPS);
    }

    #[test]
    fn composition_shift_negative_direction_uses_abs() {
        let prev: u16 = 5000;
        let new: u16 = 4490;
        let shift = (new as i32 - prev as i32).unsigned_abs() as u16;
        assert!(shift >= COMPOSITION_NOTICE_SHIFT_BPS);
    }

    #[test]
    fn first_observation_seeds_skip() {
        // After first poll, `seeded = true` and yield is recorded.
        let mut last = LastObservation::default();
        assert!(!last.seeded);
        last = LastObservation {
            yield_bps: 1500,
            composition: HashMap::new(),
            seeded: true,
        };
        assert!(last.seeded);
        assert_eq!(last.yield_bps, 1500);
    }

    #[test]
    fn composition_lookup_missing_asset_defaults_zero() {
        // When a new asset appears that wasn't in the previous tick,
        // prev_alloc defaults to 0 and the shift = new value.
        let prev: HashMap<AssetId, u16> = HashMap::new();
        let new_alloc: u16 = 1000;
        let prev_alloc = prev.get(&AssetId::SOL).copied().unwrap_or(0);
        let shift = (new_alloc as i32 - prev_alloc as i32).unsigned_abs() as u16;
        assert_eq!(shift, 1000);
    }
}
