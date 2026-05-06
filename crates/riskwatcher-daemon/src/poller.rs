//! M4: periodic Kamino obligation poller.
//!
//! Snapshots [`ObservedPositions`] every `interval` and refreshes each
//! entry from on-chain Kamino state via read-only RPC. Updates carry
//! [`Source::Poll`] and overwrite the M3 stub `obligation_pubkey` with
//! the real PDA derived from `(subject, KAMINO_MAIN_MARKET)`.
//!
//! Non-goals (deferred to later milestones):
//!   - classification (M5 thresholds)
//!   - escalate emission (M6)
//!
//! Failure policy: any RPC error against a single position is logged at
//! `warn!` and skipped. The poller never panics, never bails out of the
//! outer loop, and never short-circuits the rest of the snapshot — one
//! flaky obligation must not silence the others.
//!
//! Empty-registry contract: if there is nothing to poll, the loop body
//! returns before constructing any RPC call. This is asserted in unit
//! tests so we cannot accidentally regress it.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info, warn};

use zerox1_defi_protocols::constants::KAMINO_MAIN_MARKET;
use zerox1_defi_protocols::protocols::kamino::derive_user_obligation;
use zerox1_defi_protocols::protocols::kamino_loader::query_position_ltv_bps;
use zerox1_defi_runtime::rpc::RpcContext;

use crate::state::{ObservedPositions, PositionView, Source};

/// Drive the poll loop forever. Cancels when the future is dropped.
///
/// First tick happens after `interval` has elapsed (not immediately at
/// boot) — the registry is empty at startup and an immediate tick would
/// just be a no-op log line.
pub async fn run(
    rpc: Arc<RpcContext>,
    state: Arc<ObservedPositions>,
    interval: Duration,
) -> Result<()> {
    info!(?interval, "kamino poller starting");
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Discard the immediate first tick from `tokio::time::interval`.
    tick.tick().await;

    loop {
        tick.tick().await;
        poll_tick(&rpc, &state).await;
    }
}

/// One pass over the registry snapshot. Extracted so an empty-registry
/// snapshot is testable without a live RpcClient.
async fn poll_tick(rpc: &RpcContext, state: &ObservedPositions) {
    let snapshot = state.list().await;
    if snapshot.is_empty() {
        debug!("poll tick: registry empty, skipping");
        return;
    }
    debug!(n = snapshot.len(), "poll tick");
    for view in snapshot {
        poll_one(rpc, state, &view).await;
    }
}

/// Refresh a single [`PositionView`] in place. RPC failures are logged
/// at `warn!` and swallowed.
async fn poll_one(rpc: &RpcContext, state: &ObservedPositions, view: &PositionView) {
    let user = Pubkey::new_from_array(view.subject);
    let obligation = derive_user_obligation(&user, &KAMINO_MAIN_MARKET);

    let ltv_bps = match query_position_ltv_bps(&rpc.client, user, KAMINO_MAIN_MARKET).await {
        Ok(ltv) => ltv,
        Err(e) => {
            warn!(
                subject = %hex::encode(view.subject),
                obligation = %obligation,
                error = %e,
                "kamino LTV poll failed; skipping",
            );
            return;
        }
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let updated = PositionView {
        subject: view.subject,
        // M3 left this as `Pubkey::default()`; we now know the real PDA.
        obligation_pubkey: obligation,
        last_ltv_bps: ltv_bps,
        last_seen_unix: now,
        source: Source::Poll,
    };

    info!(
        subject = %hex::encode(view.subject),
        obligation = %obligation,
        ltv_bps,
        "kamino poll updated",
    );
    state.upsert(updated).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Empty-registry promise: `poll_tick` returns without ever touching
    /// the RPC client. We assert this by passing a context wired to an
    /// unreachable URL — if we actually issued an RPC call, the call
    /// would either error (and `poll_one`'s `warn!` would fire, but the
    /// test only proves no panic) or hang.
    ///
    /// The strong guarantee here is the structural one: `poll_tick`
    /// short-circuits on `snapshot.is_empty()` before any RPC call site
    /// is reached. This test is a smoke test of that path.
    #[tokio::test]
    async fn empty_registry_skips_rpc() {
        use solana_sdk::commitment_config::CommitmentConfig;

        let rpc = RpcContext::new(
            // Deliberately unreachable. If the implementation ever
            // tries to issue an RPC call here, the test would either
            // hang or fail — both surface a regression.
            "http://127.0.0.1:1".to_string(),
            CommitmentConfig::confirmed(),
        );
        let state = ObservedPositions::new();

        // Wrap in a short timeout. An empty-registry tick must finish
        // well under 100ms — anywhere near the timeout means we're
        // hitting RPC.
        let result =
            tokio::time::timeout(Duration::from_millis(100), poll_tick(&rpc, &state)).await;
        assert!(
            result.is_ok(),
            "poll_tick on empty registry must return immediately, not block on RPC",
        );
        assert!(state.is_empty().await, "registry must remain empty");
    }
}
