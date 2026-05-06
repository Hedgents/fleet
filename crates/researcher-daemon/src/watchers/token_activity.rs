//! Bags.fm token activity watcher. Subscribes to Solana program logs
//! for the Bags.fm launchpad program; decodes new-mint and large-trade
//! events; emits NewTokenLaunched + LargeTokenTrade MarketSignals.
//!
//! ## v0 stub
//!
//! Bags.fm log layout isn't yet decoded in this fleet, and a real
//! WebSocket `logs_subscribe` requires an additional WS RPC client
//! (solana-pubsub-client). v0 ships the structural scaffold:
//!
//! - The watcher loop is wired into `main.rs`'s `tokio::select!`.
//! - `--bags-program-id` is parsed and stored.
//! - Signal-emission helpers (`build_new_token_signal`,
//!   `build_large_trade_signal`) and the size-classifier
//!   (`classify_trade_size`) are real and tested.
//! - The actual subscriber + decoder is deferred to a follow-up
//!   commit; the loop currently just ticks at
//!   `--token-activity-tick-secs` and logs at debug level.
//!
//! When the real subscriber lands it calls these helpers to construct
//! payloads + `broadcast_signal` to fan them out — no upstream changes
//! required.
//!
//! Read-only invariant: like every other researcher watcher this never
//! signs Solana transactions. `bags_program_id` is only used to filter
//! incoming log events.

use anyhow::Result;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::interval;
use tracing::info;

use zerox1_defi_runtime::{identity::RoleIdentity, rpc::RpcContext};
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::fleet::researcher::{AssetId, MarketSignal, SignalKind, SignalSeverity};

use crate::dedup::EmissionTracker;
use crate::signal;
use crate::telemetry::TelemetryHandle;
use crate::thresholds::{LARGE_TRADE_IMPORTANT_USDC_LAMPORTS, LARGE_TRADE_NOTICE_USDC_LAMPORTS};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    _rpc: Arc<RpcContext>,
    handle: NodeHandle,
    role: RoleIdentity,
    nonce: Arc<AtomicU64>,
    dedup: Arc<EmissionTracker>,
    bags_program_id: Option<solana_sdk::pubkey::Pubkey>,
    subscribers: Arc<tokio::sync::RwLock<Vec<[u8; 32]>>>,
    telemetry: Option<Arc<TelemetryHandle>>,
    poll_interval: Duration,
) -> Result<()> {
    if bags_program_id.is_none() {
        info!("token_activity watcher disabled (no --bags-program-id)");
        return Ok(());
    }

    let mut tick = interval(poll_interval);
    info!(
        program = ?bags_program_id,
        interval_secs = poll_interval.as_secs(),
        "token_activity watcher starting (v0 stub — Bags.fm log decode pending)"
    );

    loop {
        tick.tick().await;
        // v0 stub: once a real WS log subscription + decoder is wired,
        // this body becomes a `select!` arm consuming events from the
        // subscription. For now it only logs.
        tracing::debug!(
            "token_activity polling not yet wired (Bags.fm program logs require WS subscription — M8 v0 stub)"
        );
        // Reference all captured deps so the compiler doesn't warn about
        // unused fields and so a future implementer sees the wiring intact.
        let _ = (&handle, &role, &nonce, &dedup, &subscribers, &telemetry);
    }
}

/// Pure helper: classify a trade size in USDC lamports against thresholds.
/// Public so a future log-decoder can call it; tested in isolation.
pub fn classify_trade_size(usdc_lamports: u64) -> Option<SignalSeverity> {
    if usdc_lamports >= LARGE_TRADE_IMPORTANT_USDC_LAMPORTS {
        Some(SignalSeverity::Important)
    } else if usdc_lamports >= LARGE_TRADE_NOTICE_USDC_LAMPORTS {
        Some(SignalSeverity::Notice)
    } else {
        None
    }
}

/// Build a NewTokenLaunched signal payload — exposed so future
/// log-decoder can use the same construction. `mint` is the new token's
/// mint pubkey; `initial_liq_usdc` is initial liquidity in USDC lamports
/// (carried in `context_value`).
pub fn build_new_token_signal(mint: [u8; 32], initial_liq_usdc: u64) -> MarketSignal {
    MarketSignal {
        kind: SignalKind::NewTokenLaunched,
        asset: AssetId::Other,
        asset_mint: mint,
        measurement_bps: 0,
        severity: SignalSeverity::Info,
        raised_at_unix: now_unix(),
        context_value: initial_liq_usdc,
    }
}

/// Build a LargeTokenTrade signal payload. `trade_usdc` is the trade
/// notional in USDC lamports; severity is what `classify_trade_size`
/// returned (callers should skip emission if it returned `None`).
pub fn build_large_trade_signal(
    mint: [u8; 32],
    trade_usdc: u64,
    severity: SignalSeverity,
) -> MarketSignal {
    MarketSignal {
        kind: SignalKind::LargeTokenTrade,
        asset: AssetId::Other,
        asset_mint: mint,
        measurement_bps: 0,
        severity,
        raised_at_unix: now_unix(),
        context_value: trade_usdc,
    }
}

/// Broadcast a signal to all currently-known subscribers. Helper kept
/// `pub(crate)` (and `#[allow(dead_code)]` until the real subscriber
/// wires it in) so future log-decoder code can reuse it.
#[allow(dead_code)]
pub(crate) async fn broadcast_signal(
    handle: &NodeHandle,
    role: &RoleIdentity,
    nonce: &Arc<AtomicU64>,
    subscribers: &Arc<tokio::sync::RwLock<Vec<[u8; 32]>>>,
    telemetry: Option<&Arc<TelemetryHandle>>,
    payload: MarketSignal,
) {
    let recipients = subscribers.read().await.clone();
    if recipients.is_empty() {
        return;
    }
    let _ = signal::emit_broadcast(handle, role, nonce, &recipients, payload, telemetry).await;
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
    fn classify_below_notice_returns_none() {
        assert!(classify_trade_size(LARGE_TRADE_NOTICE_USDC_LAMPORTS - 1).is_none());
        assert!(classify_trade_size(0).is_none());
    }

    #[test]
    fn classify_at_notice() {
        assert_eq!(
            classify_trade_size(LARGE_TRADE_NOTICE_USDC_LAMPORTS).unwrap() as u8,
            SignalSeverity::Notice as u8
        );
    }

    #[test]
    fn classify_at_important() {
        assert_eq!(
            classify_trade_size(LARGE_TRADE_IMPORTANT_USDC_LAMPORTS).unwrap() as u8,
            SignalSeverity::Important as u8
        );
        // And way-above also classifies as Important (no over-saturation).
        assert_eq!(
            classify_trade_size(LARGE_TRADE_IMPORTANT_USDC_LAMPORTS * 100).unwrap() as u8,
            SignalSeverity::Important as u8
        );
    }

    #[test]
    fn build_new_token_round_trips_via_cbor() {
        let mint = [7u8; 32];
        let s = build_new_token_signal(mint, 50_000_000_000);
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&s, &mut buf).unwrap();
        let decoded: MarketSignal = ciborium::de::from_reader(&buf[..]).unwrap();
        assert_eq!(decoded, s);
        assert_eq!(decoded.asset, AssetId::Other);
        assert_eq!(decoded.asset_mint, mint);
        assert_eq!(decoded.kind, SignalKind::NewTokenLaunched);
        assert_eq!(decoded.context_value, 50_000_000_000);
    }

    #[test]
    fn build_large_trade_round_trips() {
        let mint = [42u8; 32];
        let s = build_large_trade_signal(mint, 200_000_000_000, SignalSeverity::Important);
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&s, &mut buf).unwrap();
        let decoded: MarketSignal = ciborium::de::from_reader(&buf[..]).unwrap();
        assert_eq!(decoded, s);
        assert_eq!(decoded.kind, SignalKind::LargeTokenTrade);
        assert_eq!(decoded.asset_mint, mint);
        assert_eq!(decoded.context_value, 200_000_000_000);
    }

    #[test]
    fn classify_threshold_boundaries() {
        // Just below Important should still classify as Notice.
        assert_eq!(
            classify_trade_size(LARGE_TRADE_IMPORTANT_USDC_LAMPORTS - 1).unwrap() as u8,
            SignalSeverity::Notice as u8
        );
    }
}
