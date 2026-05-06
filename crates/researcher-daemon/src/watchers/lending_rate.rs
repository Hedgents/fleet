//! Kamino lending rate watcher. Polls the configured reserves on a
//! tick, computes current borrow + supply APR, emits MarketSignal
//! when delta from last-known value crosses thresholds.
//!
//! APR derivation: M3 ships with a 0-stub. The `kamino_loader::load_reserve`
//! helper currently exposes only the pubkeys needed to build deposit/
//! withdraw instructions (no rate-curve fields). Computing real APRs
//! requires either:
//!   (a) extending `kamino_loader` to decode `ReserveLiquidity` (offsets
//!       into the 8624-byte raw account: utilization inputs + curve
//!       params + protocol take rate), or
//!   (b) a new helper that returns just the rate fields.
//!
//! Either is non-trivial reverse-engineering of klend's internal layout
//! and warrants its own milestone. For M3 we ship the watcher loop
//! (poll cadence, threshold classification, dedup, signal broadcast)
//! and stub APR to 0 — proving the emission infrastructure works
//! end-to-end. The real numbers will land in a follow-up.

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
use zerox1_protocol::fleet::researcher::{
    AssetId, MarketSignal, SignalKind, SignalSeverity,
};

use crate::dedup::EmissionTracker;
use crate::signal;
use crate::thresholds::{LENDING_RATE_INFO_DELTA_BPS, LENDING_RATE_NOTICE_DELTA_BPS};

/// One reserve to watch. `asset` is the typed enum slot;
/// `reserve_pubkey` is the on-chain account.
#[derive(Clone)]
pub struct ReserveSpec {
    pub asset: AssetId,
    pub reserve_pubkey: Pubkey,
    pub display_name: String,
}

#[derive(Clone, Copy, Default)]
struct LastKnown {
    borrow_apr_bps: i32,
    supply_apr_bps: i32,
}

/// Run the lending watcher loop. `subscribers` are the recipients to
/// broadcast signals to (typically all peer agents the researcher has
/// observed via BEACON; for v0 pass an explicit list from CLI).
pub async fn run(
    rpc: Arc<RpcContext>,
    handle: NodeHandle,
    role: RoleIdentity,
    nonce: Arc<AtomicU64>,
    dedup: Arc<EmissionTracker>,
    reserves: Vec<ReserveSpec>,
    subscribers: Arc<tokio::sync::RwLock<Vec<[u8; 32]>>>,
    poll_interval: Duration,
) -> anyhow::Result<()> {
    let mut last_known: HashMap<Pubkey, LastKnown> = HashMap::new();
    let mut tick = interval(poll_interval);

    info!(
        reserve_count = reserves.len(),
        poll_interval_secs = poll_interval.as_secs(),
        "lending_rate watcher starting"
    );

    loop {
        tick.tick().await;
        for spec in &reserves {
            match poll_one(&rpc, spec).await {
                Ok((borrow_bps, supply_bps)) => {
                    let prev = last_known
                        .get(&spec.reserve_pubkey)
                        .copied()
                        .unwrap_or_default();
                    // First observation has no baseline — skip emission, just record.
                    let is_first = !last_known.contains_key(&spec.reserve_pubkey);
                    let borrow_delta = borrow_bps - prev.borrow_apr_bps;
                    let supply_delta = supply_bps - prev.supply_apr_bps;

                    if !is_first {
                        if let Some(severity) = classify_delta(borrow_delta) {
                            if dedup.should_emit(
                                SignalKind::LendingBorrowRateAbove,
                                spec.asset,
                                severity,
                            ) {
                                let payload = MarketSignal {
                                    kind: SignalKind::LendingBorrowRateAbove,
                                    asset: spec.asset,
                                    asset_mint: [0u8; 32],
                                    measurement_bps: borrow_bps,
                                    severity,
                                    raised_at_unix: now_unix(),
                                    context_value: 0,
                                };
                                broadcast(&handle, &role, &nonce, &subscribers, payload).await;
                            }
                        }
                        if let Some(severity) = classify_delta(supply_delta) {
                            if dedup.should_emit(
                                SignalKind::LendingSupplyRateAbove,
                                spec.asset,
                                severity,
                            ) {
                                let payload = MarketSignal {
                                    kind: SignalKind::LendingSupplyRateAbove,
                                    asset: spec.asset,
                                    asset_mint: [0u8; 32],
                                    measurement_bps: supply_bps,
                                    severity,
                                    raised_at_unix: now_unix(),
                                    context_value: 0,
                                };
                                broadcast(&handle, &role, &nonce, &subscribers, payload).await;
                            }
                        }
                    }
                    last_known.insert(
                        spec.reserve_pubkey,
                        LastKnown {
                            borrow_apr_bps: borrow_bps,
                            supply_apr_bps: supply_bps,
                        },
                    );
                }
                Err(e) => {
                    // Non-fatal — log and skip this reserve this tick.
                    warn!(?e, reserve = %spec.display_name, "lending poll failed");
                }
            }
        }
    }
}

/// Poll a single reserve. Returns `(borrow_apr_bps, supply_apr_bps)`.
///
/// M3 v0: fetches the raw Reserve account to verify it exists + decodes
/// at the right size, then returns 0/0. APR derivation from the raw
/// reserve bytes is a follow-up (see module-level docstring).
async fn poll_one(rpc: &RpcContext, spec: &ReserveSpec) -> Result<(i32, i32)> {
    // Fetch raw account data — proves the reserve is reachable and the
    // RPC works. We do NOT use `kamino_loader::load_reserve` here
    // because it requires `liquidity_mint` + `expected_lending_market`
    // (only relevant for instruction building) and APR-watcher input
    // ergonomics shouldn't carry that overhead.
    let data = rpc
        .client
        .get_account_data(&spec.reserve_pubkey)
        .await
        .with_context(|| format!("fetch reserve {}", spec.reserve_pubkey))?;

    let borrow_apr_bps = compute_borrow_apr_bps(&data)?;
    let supply_apr_bps = compute_supply_apr_bps(&data, borrow_apr_bps)?;
    Ok((borrow_apr_bps, supply_apr_bps))
}

/// v0 stub. See module-level docstring for the gap.
fn compute_borrow_apr_bps(_reserve_data: &[u8]) -> Result<i32> {
    // TODO(M3-polish): decode klend Reserve interest-rate model:
    //   utilization = total_borrows / total_supplied
    //   borrow_apr  = piecewise-linear over utilization using
    //                 (min_borrow_rate, optimal_borrow_rate, max_borrow_rate,
    //                  optimal_utilization). Curve points live in the
    //                 reserve config block at offsets to be reverse-
    //                 engineered against the klend source.
    Ok(0)
}

/// v0 stub. See module-level docstring for the gap.
fn compute_supply_apr_bps(_reserve_data: &[u8], _borrow_apr: i32) -> Result<i32> {
    // TODO(M3-polish): supply_apr = utilization × borrow_apr × (1 - protocol_take_rate).
    Ok(0)
}

/// Map a basis-points delta to a severity band (or None if below threshold).
fn classify_delta(delta_bps: i32) -> Option<SignalSeverity> {
    let abs = delta_bps.abs();
    if abs >= LENDING_RATE_NOTICE_DELTA_BPS as i32 {
        Some(SignalSeverity::Notice)
    } else if abs >= LENDING_RATE_INFO_DELTA_BPS as i32 {
        Some(SignalSeverity::Info)
    } else {
        None
    }
}

async fn broadcast(
    handle: &NodeHandle,
    role: &RoleIdentity,
    nonce: &Arc<AtomicU64>,
    subscribers: &Arc<tokio::sync::RwLock<Vec<[u8; 32]>>>,
    payload: MarketSignal,
) {
    let recipients = subscribers.read().await.clone();
    if recipients.is_empty() {
        info!(
            kind = ?payload.kind,
            asset = ?payload.asset,
            "lending signal generated but no subscribers — skipping broadcast"
        );
        return;
    }
    let sent = signal::emit_broadcast(handle, role, nonce, &recipients, payload).await;
    info!(
        sent_count = sent,
        recipient_count = recipients.len(),
        "lending signal broadcast"
    );
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse a `name:base58_pubkey:asset_enum` reserve spec string.
/// `asset_enum` is one of: `SOL`, `ETH`, `BTC`, `USDC`, `USDT`, `JLP`,
/// `Other` (case-sensitive).
pub fn parse_reserve_spec(s: &str) -> Result<ReserveSpec> {
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    if parts.len() != 3 {
        anyhow::bail!(
            "reserve spec must be `name:base58_pubkey:asset_enum`, got {s:?}"
        );
    }
    let name = parts[0].to_string();
    let pubkey: Pubkey = parts[1]
        .parse()
        .with_context(|| format!("parsing reserve pubkey {:?}", parts[1]))?;
    let asset = parse_asset_id(parts[2])?;
    Ok(ReserveSpec {
        asset,
        reserve_pubkey: pubkey,
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
    fn classify_delta_below_info_returns_none() {
        assert!(classify_delta(40).is_none());
        assert!(classify_delta(-40).is_none());
    }

    #[test]
    fn classify_delta_at_info_band() {
        assert_eq!(
            classify_delta(50).unwrap() as u8,
            SignalSeverity::Info as u8
        );
        assert_eq!(
            classify_delta(-100).unwrap() as u8,
            SignalSeverity::Info as u8
        );
    }

    #[test]
    fn classify_delta_at_notice_band() {
        assert_eq!(
            classify_delta(200).unwrap() as u8,
            SignalSeverity::Notice as u8
        );
        assert_eq!(
            classify_delta(-500).unwrap() as u8,
            SignalSeverity::Notice as u8
        );
    }

    #[test]
    fn classify_delta_zero_returns_none() {
        assert!(classify_delta(0).is_none());
    }

    #[test]
    fn parse_reserve_spec_round_trips() {
        let spec = parse_reserve_spec("usdc:11111111111111111111111111111111:USDC")
            .expect("parse");
        assert_eq!(spec.display_name, "usdc");
        assert_eq!(spec.asset as u16, AssetId::USDC as u16);
        assert_eq!(spec.reserve_pubkey, Pubkey::default());
    }

    #[test]
    fn parse_reserve_spec_rejects_bad_format() {
        assert!(parse_reserve_spec("only:two").is_err());
        assert!(parse_reserve_spec("name:notabase58:USDC").is_err());
        assert!(parse_reserve_spec("name:11111111111111111111111111111111:WAT").is_err());
    }
}
