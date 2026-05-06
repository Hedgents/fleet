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

/// Outcome of a single per-position poll. Aggregated by `poll_tick`
/// into a single `info!` summary line per tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollOutcome {
    /// RPC succeeded and the registry was upserted with `Source::Poll`.
    Updated,
    /// RPC failed; the existing entry is left untouched.
    Skipped,
}

/// One pass over the registry snapshot. Extracted so an empty-registry
/// snapshot is testable without a live RpcClient.
///
/// Per-position RPC calls are fanned out concurrently via
/// `futures::future::join_all`. Concurrency is naturally bounded by
/// [`REGISTRY_CAPACITY`] (32), so no semaphore is needed. A single
/// per-tick `info!` summary is emitted after all polls complete; the
/// per-position detail lives at `debug!`.
async fn poll_tick(rpc: &RpcContext, state: &ObservedPositions) {
    let snapshot = state.list().await;
    if snapshot.is_empty() {
        debug!("poll tick: registry empty, skipping");
        return;
    }
    let n_total = snapshot.len();
    debug!(n = n_total, "poll tick");

    let futures = snapshot.iter().map(|view| poll_one(rpc, state, view));
    let outcomes = futures::future::join_all(futures).await;

    let n_ok = outcomes
        .iter()
        .filter(|o| **o == PollOutcome::Updated)
        .count();
    let n_skipped = n_total - n_ok;
    info!(n_total, n_ok, n_skipped, "poll tick complete");
}

/// Refresh a single [`PositionView`] in place. RPC failures are logged
/// at `warn!` and swallowed; success is logged at `debug!` and the
/// caller aggregates outcomes into a per-tick summary.
async fn poll_one(
    rpc: &RpcContext,
    state: &ObservedPositions,
    view: &PositionView,
) -> PollOutcome {
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
            return PollOutcome::Skipped;
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

    debug!(
        subject = %hex::encode(view.subject),
        obligation = %obligation,
        ltv_bps,
        "kamino poll updated",
    );
    state.upsert(updated).await;
    PollOutcome::Updated
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

    /// Failure-path contract: when the registry is non-empty and the
    /// RPC is unreachable, `poll_tick` must:
    ///   1. attempt the RPC (otherwise the test below is meaningless),
    ///   2. log a warn and skip on error,
    ///   3. NOT upsert (so existing data is preserved with its original
    ///      `source` and `last_seen_unix`).
    ///
    /// Pre-populating with `Source::Report` lets us assert (3) directly:
    /// if the failure path silently fell through to upsert, the source
    /// would flip to `Source::Poll`. We also pin `last_seen_unix` to a
    /// sentinel value so a clobbered timestamp is detectable.
    #[tokio::test]
    async fn non_empty_registry_failure_does_not_upsert() {
        use solana_sdk::commitment_config::CommitmentConfig;

        let rpc = RpcContext::new(
            // Same unreachable URL as the empty-registry test; here we
            // deliberately want the RPC attempt to FAIL so we can
            // observe the warn-and-skip path.
            "http://127.0.0.1:1".to_string(),
            CommitmentConfig::confirmed(),
        );
        let state = ObservedPositions::new();

        let subject: [u8; 32] = [7u8; 32];
        const SENTINEL_TS: u64 = 111_111_111;
        let original = PositionView {
            subject,
            obligation_pubkey: Pubkey::default(),
            last_ltv_bps: 4242,
            last_seen_unix: SENTINEL_TS,
            source: Source::Report,
        };
        state.upsert(original.clone()).await;

        // Wrap in a generous timeout. Connection-refused on
        // 127.0.0.1:1 returns immediately; a hang would mean we are
        // not actually short-circuiting on RPC failure.
        let result =
            tokio::time::timeout(Duration::from_secs(5), poll_tick(&rpc, &state)).await;
        assert!(
            result.is_ok(),
            "poll_tick must return promptly even when RPC fails",
        );

        let entries = state.list().await;
        assert_eq!(entries.len(), 1, "registry size must be unchanged");
        let after = entries.into_iter().next().unwrap();
        assert_eq!(after.subject, subject, "subject must match");
        assert_eq!(
            after.source,
            Source::Report,
            "source must NOT have flipped to Poll — failed RPC must not upsert",
        );
        assert_eq!(
            after.last_seen_unix, SENTINEL_TS,
            "last_seen_unix must be preserved on RPC failure",
        );
        assert_eq!(
            after.last_ltv_bps, 4242,
            "last_ltv_bps must be preserved on RPC failure",
        );
    }
}
