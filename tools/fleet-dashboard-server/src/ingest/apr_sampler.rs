//! Background sampler that snapshots live APR (basis points) into the
//! `apr_samples` table every 60 seconds. Powers `/apr/history`.
//!
//! Sources:
//! - `stable_yield` → Kamino USDC supply APR via `chain::rates`
//!   (`source = kamino_reserve`).
//! - `multiply`, `hedgedjlp` → the daemon's most recent `pnl_snapshot`
//!   row's APR field. Paper-only rows (filtered by
//!   `api::state::is_paper_row`) are skipped — we only sample APR when
//!   it reflects a real on-chain position (`source = daemon_telemetry`).
//!
//! Failure mode: every error path here is logged and swallowed. RPC
//! outages or missing telemetry must never crash the dashboard.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::api::state::is_paper_row;
use crate::chain::ChainReader;
use crate::store::Store;

/// Polling cadence. 60s is intentional: the underlying rate
/// `RATES_CACHE_TTL` is 5 min and daemon `pnl_snapshot` rows are written
/// every few seconds, so 60s gives ~1,440 rows per strategy per day and
/// keeps storage bounded at a few hundred KB per week.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(60);

/// Drives the sampler forever.
pub async fn run(store: Arc<Store>, chain: Arc<ChainReader>) {
    // Stagger the first sample a few seconds after boot so chain reads
    // don't race with the rest of the startup path.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let mut tick = tokio::time::interval(SAMPLE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        sample_once(&store, &chain).await;
    }
}

async fn sample_once(store: &Arc<Store>, chain: &Arc<ChainReader>) {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // stable_yield: live Kamino USDC supply APR.
    let rates = chain.rate_snapshot().await;
    if rates.kamino_usdc_supply_bps > 0 {
        if let Err(e) = store
            .insert_apr_sample(
                now_ms,
                "stable_yield",
                rates.kamino_usdc_supply_bps as i64,
                "kamino_reserve",
            )
            .await
        {
            tracing::warn!(?e, "insert stable_yield apr sample");
        }
    } else {
        tracing::debug!("skipping stable_yield apr sample — supply_bps=0 (rpc down?)");
    }

    // multiply / hedgedjlp: most recent non-paper pnl_snapshot row.
    for (daemon, apr_field) in [
        ("multiply", "multiply_net_apr_bps"),
        ("hedgedjlp", "hedgedjlp_net_apr_bps"),
    ] {
        match store.recent_pnl_for(daemon, 5).await {
            Ok(rows) => {
                // Walk newest-first looking for a row that isn't paper-only.
                let newest_real = rows.iter().rev().find(|(_, json)| !is_paper_row(json));
                if let Some((_, json)) = newest_real {
                    let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
                    if let Some(bps) = v.get(apr_field).and_then(|x| x.as_i64()) {
                        if let Err(e) = store
                            .insert_apr_sample(now_ms, daemon, bps, "daemon_telemetry")
                            .await
                        {
                            tracing::warn!(?e, daemon, "insert apr sample");
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(?e, daemon, "apr sampler: recent_pnl_for failed"),
        }
    }
}
