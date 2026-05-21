//! Background sampler that snapshots chain-state AUM into the
//! `chain_aum_snapshots` table every 60 seconds. Powers the rc24
//! rewrite of `/pnl`.
//!
//! ## Why not per-daemon telemetry?
//!
//! Pre-rc24 `/pnl` aggregated rows written by each daemon's own
//! telemetry loop. That worked when every daemon was running in live
//! mode AND its internal `ActivePosition` tracked the on-chain truth.
//! In production it failed silently in three known ways:
//!
//!   1. The hedgedjlp daemon's `ActivePosition` desynced from chain
//!      after a partial unwind, so its telemetry reported
//!      `jlp_value_usd_micro: 0` even when the chain still held the
//!      position.
//!   2. `multiply` / `stable_yield` were sometimes run in paper-mode
//!      systemd units that write `*-pnl.jsonl` (not
//!      `*-live-pnl.jsonl`), so /pnl scanning the live path saw
//!      multi-day-stale data while the daemons were happily polling
//!      and reporting real positions.
//!   3. A daemon that hadn't booted yet within the window made the
//!      whole aggregate degenerate to zero.
//!
//! The fix is to read AUM the same way `/aum` already does — directly
//! from chain via the dashboard's chain reader modules — and snapshot
//! it on a fixed cadence. /pnl then computes deltas across snapshots,
//! never trusting per-daemon telemetry.
//!
//! ## Cadence
//!
//! 60s — matches `apr_sampler::SAMPLE_INTERVAL`. The Solana RPC reads
//! we make (Kamino obligations, Jupiter Perps positions, wallet ATA
//! balances) take ~1–3s under normal latency, so 60s gives ~1,440 rows
//! per day and a few hundred KB of growth per week. Coarser than this
//! would lose intra-hour fidelity; finer would inflate the SQLite file
//! without operationally useful gain — protocols only update yields on
//! the minute timescale anyway.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use solana_sdk::pubkey::Pubkey;
use tracing::{debug, warn};

use crate::api::state::read_chain_aum_breakdown;
use crate::chain::ChainReader;
use crate::store::Store;

/// Polling cadence. Mirrors `apr_sampler::SAMPLE_INTERVAL` so the two
/// sampler loops share the same disk-growth profile.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(60);

/// Initial delay after boot before the first sample fires. Lets the
/// dashboard's HTTP server bind, the chain reader prime its caches,
/// and the rest of the startup path settle before issuing RPC reads.
const STARTUP_GRACE: Duration = Duration::from_secs(5);

/// Drives the sampler forever. Spawned by `main::run` alongside the
/// APR sampler.
pub async fn run(store: Arc<Store>, chain: Arc<ChainReader>, wallet: Pubkey) {
    tokio::time::sleep(STARTUP_GRACE).await;
    // Take one snapshot immediately after the grace period so /pnl has
    // SOMETHING to bracket against on a fresh boot, even before the
    // first scheduled tick.
    sample_once(&store, &chain, &wallet).await;
    let mut tick = tokio::time::interval(SAMPLE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first `tick().await` returns immediately, so consume it so
    // the second tick is the first *post-startup* one.
    tick.tick().await;
    loop {
        tick.tick().await;
        sample_once(&store, &chain, &wallet).await;
    }
}

async fn sample_once(store: &Arc<Store>, chain: &Arc<ChainReader>, wallet: &Pubkey) {
    let breakdown = read_chain_aum_breakdown(chain, wallet).await;
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Refuse to write a snapshot with total_usd == 0 — that's almost
    // certainly an RPC failure rather than a real zero-AUM state. The
    // sampler will retry on the next tick.
    if breakdown.total_usd() <= 0.0 {
        debug!(
            "aum_sampler: chain reads returned $0 total — skipping snapshot \
             (likely transient RPC failure)"
        );
        return;
    }

    if let Err(e) = store
        .insert_chain_aum_snapshot(
            now_unix,
            breakdown.total_usd(),
            breakdown.multiply_usd,
            breakdown.stable_yield_usd,
            breakdown.hedgedjlp_jlp_usd,
            breakdown.hedgedjlp_collateral_usd,
            breakdown.idle_usd,
        )
        .await
    {
        warn!(?e, "aum_sampler: insert_chain_aum_snapshot failed");
    } else {
        debug!(
            total_usd = breakdown.total_usd(),
            ts_unix = now_unix,
            "aum_sampler: snapshot written"
        );
    }
}
