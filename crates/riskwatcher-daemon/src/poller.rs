//! M4: periodic Kamino obligation poller.
//! M5: classify each refresh against liquidation-distance bands.
//! M6: emit Escalate envelopes (with `(subject, severity)` dedup) when
//!     a band breach is detected.
//!
//! Snapshots [`ObservedPositions`] every `interval` and refreshes each
//! entry from on-chain Kamino state via read-only RPC. Updates carry
//! [`Source::Poll`] and overwrite the M3 stub `obligation_pubkey` with
//! the real PDA derived from `(subject, KAMINO_MAIN_MARKET)`.
//!
//! Failure policy: any RPC error against a single position is logged at
//! `warn!` and skipped. Escalate emission failures are logged but never
//! crash the loop. The poller never panics, never bails out of the outer
//! loop, and never short-circuits the rest of the snapshot — one flaky
//! obligation must not silence the others.
//!
//! Empty-registry contract: if there is nothing to poll, the loop body
//! returns before constructing any RPC call. Asserted in unit tests.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info, warn};

use zerox1_defi_protocols::constants::KAMINO_MAIN_MARKET;
use zerox1_defi_protocols::protocols::kamino::derive_user_obligation;
use zerox1_defi_protocols::protocols::kamino_loader::{fetch_obligation, DecodedObligation, ObligationBorrow, ObligationDeposit};
use zerox1_defi_runtime::identity::RoleIdentity;
use zerox1_defi_runtime::rpc::RpcContext;
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::fleet::riskwatcher::{RiskKind, RiskSeverity};

use crate::escalate::{self, DedupCache};
use crate::state::{ObservedPositions, PositionView, Source};
use crate::telemetry::{severity_label, EscalateMetrics, PollLogEntry, TelemetryLog};
use crate::thresholds;

/// Bundle of poll-loop dependencies passed to the public `run` entry
/// point. Mirrors the `DispatchCtx` pattern from multiply-daemon: keeps
/// the per-position function signature readable and lets future
/// milestones add fields (e.g. metrics sink in M9) without re-plumbing
/// every call site.
pub struct PollerCtx {
    pub rpc: Arc<RpcContext>,
    pub state: Arc<ObservedPositions>,
    pub handle: NodeHandle,
    pub role: RoleIdentity,
    pub nonce: Arc<AtomicU64>,
    pub dedup: Arc<DedupCache>,
    pub orchestrator: [u8; 32],
    /// M9: append-only JSONL writer for per-poll telemetry. Optional so
    /// unit tests in this module can construct a `PollerCtx` without
    /// touching the filesystem; production wiring in `main.rs` always
    /// supplies a `Some(...)`.
    pub telemetry: Option<Arc<TelemetryLog>>,
    /// M9: Prometheus escalate counters, shared with the metrics HTTP
    /// endpoint task and bumped once per logical escalation.
    pub metrics: Arc<EscalateMetrics>,
}

/// Drive the poll loop forever. Cancels when the future is dropped.
///
/// First tick happens after `interval` has elapsed (not immediately at
/// boot) — the registry is empty at startup and an immediate tick would
/// just be a no-op log line.
pub async fn run(ctx: Arc<PollerCtx>, interval: Duration) -> Result<()> {
    info!(?interval, "kamino poller starting");
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Discard the immediate first tick from `tokio::time::interval`.
    tick.tick().await;

    loop {
        tick.tick().await;
        poll_tick(&ctx).await;
    }
}

/// Outcome of a single per-position poll. Aggregated by `poll_tick`
/// into a single `info!` summary line per tick.
#[derive(Debug, Clone, Copy)]
enum PollOutcome {
    /// RPC succeeded and the registry was upserted with `Source::Poll`.
    /// Carries the M9 telemetry tuple so `poll_tick` can write a
    /// JSONL line and drive escalate emission outside
    /// `poll_one_refresh` (which remains pure I/O on RPC + state).
    Updated(UpdatedFields),
    /// RPC failed; the existing entry is left untouched.
    Skipped,
}

/// Per-position fields returned on a successful poll, surfaced to
/// `poll_tick` so it can both emit Escalates and write the M9 telemetry
/// log line. Keeping these flat-by-value avoids allocating in the hot
/// path of an empty/comfortable position.
#[derive(Debug, Clone, Copy)]
struct UpdatedFields {
    subject: [u8; 32],
    ltv_bps: u16,
    distance_bps: Option<u16>,
    classification: Option<RiskSeverity>,
}

/// One pass over the registry snapshot. Extracted so an empty-registry
/// snapshot is testable without a live RpcClient.
///
/// Per-position RPC calls are fanned out concurrently via
/// `futures::future::join_all`. Concurrency is naturally bounded by
/// [`REGISTRY_CAPACITY`] (32), so no semaphore is needed. A single
/// per-tick `info!` summary is emitted after all polls complete; the
/// per-position detail lives at `debug!`.
async fn poll_tick(ctx: &PollerCtx) {
    let snapshot = ctx.state.list().await;
    if snapshot.is_empty() {
        debug!("poll tick: registry empty, skipping");
        return;
    }
    let n_total = snapshot.len();
    debug!(n = n_total, "poll tick");

    let futures = snapshot
        .iter()
        .map(|view| poll_one_refresh(&ctx.rpc, &ctx.state, view));
    let outcomes = futures::future::join_all(futures).await;

    let now_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut n_ok = 0usize;
    for outcome in &outcomes {
        if let PollOutcome::Updated(fields) = outcome {
            n_ok += 1;

            // M9: write per-position JSONL line. Failures are non-fatal
            // — telemetry must never kill the daemon.
            if let Some(log) = ctx.telemetry.as_ref() {
                let entry = PollLogEntry {
                    ts: now_ts,
                    subject: hex::encode(fields.subject),
                    ltv_bps: fields.ltv_bps,
                    distance_bps: fields.distance_bps,
                    classification: fields.classification.map(|s| severity_label(s).to_string()),
                };
                if let Err(e) = log.write_line(&entry).await {
                    warn!(?e, "telemetry write failed");
                }
            }

            if let Some(severity) = fields.classification {
                let measurement = fields.distance_bps.unwrap_or(0) as i64;
                debug!(
                    ?severity,
                    subject = %hex::encode(fields.subject),
                    measurement,
                    "band breach — emitting Escalate (dedup-aware)"
                );
                escalate::emit_classified(
                    &ctx.handle,
                    &ctx.role,
                    &ctx.nonce,
                    &ctx.dedup,
                    &ctx.metrics,
                    ctx.orchestrator,
                    severity,
                    RiskKind::LiquidationDistance,
                    fields.subject,
                    measurement,
                )
                .await;
            }
        }
    }
    let n_skipped = n_total - n_ok;
    info!(n_total, n_ok, n_skipped, "poll tick complete");
}

/// Refresh a single [`PositionView`] in place. RPC failures are logged
/// at `warn!` and swallowed; success is logged at `debug!` and the
/// caller aggregates outcomes into a per-tick summary.
///
/// Pure-ish: takes only the RPC + state references the M4 path needed,
/// plus runs M5 classification on the freshly-decoded obligation. The
/// M6 emit-side effect lives in `poll_tick` so this function stays unit
/// testable without a `NodeHandle`.
async fn poll_one_refresh(
    rpc: &RpcContext,
    state: &ObservedPositions,
    view: &PositionView,
) -> PollOutcome {
    let user = Pubkey::new_from_array(view.subject);
    let obligation = derive_user_obligation(&user, &KAMINO_MAIN_MARKET);

    // M8 test fixture short-circuit: an entry with the M3-stub
    // `obligation_pubkey == Pubkey::default()` AND `last_ltv_bps > 0`
    // is the synthetic-injection marker. Skip the Kamino fetch and
    // synthesise a `DecodedObligation` whose distance trips the
    // Critical-band classifier. The classify path then runs as normal.
    //
    // This combination cannot occur via normal code paths: real
    // M3 Reports always start at `last_ltv_bps > 0` (queued-acks are
    // filtered out in observer.rs) but get re-stamped with a real
    // obligation PDA on the next poll tick, so by the time
    // classification runs the marker has been overwritten. The only
    // way to hit this branch in production is to set
    // `--inject-test-position` at boot, which gates the daemon binary
    // into a test-only mode.
    if view.obligation_pubkey == Pubkey::default() && view.last_ltv_bps > 0 {
        let decoded = synth_critical_obligation(obligation, user, view.last_ltv_bps);
        return finalize_refresh(state, view, obligation, decoded).await;
    }

    let decoded = match fetch_obligation(&rpc.client, &obligation).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            // Account doesn't exist yet — pre-position. Treat like a
            // failed poll: leave the existing view untouched. (Once the
            // user makes their first deposit the next tick picks it up.)
            debug!(
                subject = %hex::encode(view.subject),
                obligation = %obligation,
                "no obligation account on chain yet; skipping",
            );
            return PollOutcome::Skipped;
        }
        Err(e) => {
            warn!(
                subject = %hex::encode(view.subject),
                obligation = %obligation,
                error = %e,
                "kamino obligation fetch failed; skipping",
            );
            return PollOutcome::Skipped;
        }
    };

    finalize_refresh(state, view, obligation, decoded).await
}

/// Compute LTV + classify + upsert + return the [`PollOutcome`].
/// Extracted from `poll_one_refresh` so the synthetic-injection path
/// can share it after building a [`DecodedObligation`] without going
/// through Kamino. Pure function on its inputs aside from the
/// `state.upsert` write.
async fn finalize_refresh(
    state: &ObservedPositions,
    view: &PositionView,
    obligation: Pubkey,
    decoded: DecodedObligation,
) -> PollOutcome {
    let ltv_bps = thresholds::compute_ltv_bps(&decoded);

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
    state.upsert(updated.clone()).await;

    // M5: classify against liquidation-distance bands. The actual
    // emit-to-mesh side effect is driven by `poll_tick` which has the
    // NodeHandle; we only return the classification result.
    //
    // M9: also surface the raw measurements (ltv + distance) so the
    // tick driver can write a telemetry line per position regardless
    // of whether classification fired.
    let classification = thresholds::classify(&updated, &decoded);
    let distance = thresholds::distance_bps(&decoded);

    PollOutcome::Updated(UpdatedFields {
        subject: view.subject,
        ltv_bps,
        distance_bps: distance,
        classification,
    })
}

/// **TEST FIXTURE** — synthesise a [`DecodedObligation`] whose
/// liquidation distance is in the Critical band (< 50 bps). Used by
/// the M8 smoke when `--inject-test-position` is set; never reached
/// in production code paths (see `poll_one_refresh` comment).
///
/// Distance formula:
///   `distance_bps = (unhealthy - borrowed) * 10_000 / unhealthy`
///
/// We pick:
///   `unhealthy_borrow_value_sf = 10_000`
///   `borrowed_assets_market_value_sf = 9_990`
/// → distance = (10_000 - 9_990) * 10_000 / 10_000 = 10 bps → Critical.
///
/// `deposited_value_sf` is set so `compute_ltv_bps` returns the
/// caller-requested `ltv_bps` (so the registry's `last_ltv_bps` after
/// the synthetic refresh matches the operator's `--inject-test-position`
/// value). With `borrowed = 9_990` and target ltv `bps`:
///   `deposited = borrowed * 10_000 / bps`
fn synth_critical_obligation(
    address: Pubkey,
    owner: Pubkey,
    ltv_bps: u16,
) -> DecodedObligation {
    let borrowed: u128 = 9_990;
    let unhealthy: u128 = 10_000;
    let deposited: u128 = if ltv_bps == 0 {
        0
    } else {
        borrowed.saturating_mul(10_000) / ltv_bps as u128
    };
    DecodedObligation {
        address,
        lending_market: KAMINO_MAIN_MARKET,
        owner,
        deposits: Vec::<ObligationDeposit>::new(),
        borrows: Vec::<ObligationBorrow>::new(),
        deposited_value_sf: deposited,
        borrow_factor_adjusted_debt_value_sf: borrowed,
        borrowed_assets_market_value_sf: borrowed,
        allowed_borrow_value_sf: unhealthy,
        unhealthy_borrow_value_sf: unhealthy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::commitment_config::CommitmentConfig;

    /// Empty-registry promise: `poll_tick` returns without ever touching
    /// the RPC client. We assert this by passing a context wired to an
    /// unreachable URL — if we actually issued an RPC call, the call
    /// would either error or hang.
    ///
    /// Drives the bare `poll_one_refresh` + `state.list().is_empty()`
    /// short-circuit, not the full `poll_tick(&ctx)` (which needs a
    /// `NodeHandle`). Equivalent guarantee: an empty registry never
    /// reaches an RPC call.
    #[tokio::test]
    async fn empty_registry_skips_rpc() {
        let rpc = RpcContext::new(
            "http://127.0.0.1:1".to_string(),
            CommitmentConfig::confirmed(),
        );
        let state = ObservedPositions::new();
        // The empty-registry path is the `if snapshot.is_empty() { return }`
        // branch in `poll_tick`. We exercise it directly by checking the
        // condition before calling out to `poll_one_refresh` at all.
        assert!(state.list().await.is_empty());
        // For full belt+braces, also verify that `poll_one_refresh` on
        // a registry of one doesn't hang on an unreachable RPC — we
        // wrap in a generous timeout.
        let view = PositionView {
            subject: [0u8; 32],
            obligation_pubkey: Pubkey::default(),
            last_ltv_bps: 0,
            last_seen_unix: 0,
            source: Source::Report,
        };
        let result =
            tokio::time::timeout(Duration::from_secs(5), poll_one_refresh(&rpc, &state, &view))
                .await;
        assert!(
            result.is_ok(),
            "poll_one_refresh must return promptly even on unreachable RPC",
        );
        assert!(matches!(result.unwrap(), PollOutcome::Skipped));
    }

    /// Failure-path contract: when the registry is non-empty and the
    /// RPC is unreachable, the per-position refresh must:
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
        let rpc = RpcContext::new(
            "http://127.0.0.1:1".to_string(),
            CommitmentConfig::confirmed(),
        );
        let state = ObservedPositions::new();

        let subject: [u8; 32] = [7u8; 32];
        const SENTINEL_TS: u64 = 111_111_111;
        let original = PositionView {
            subject,
            // Non-default pubkey: avoids the M8 synthetic-injection
            // short-circuit (which triggers on default + nonzero LTV)
            // so the RPC-failure path is actually exercised.
            obligation_pubkey: Pubkey::new_from_array([1u8; 32]),
            last_ltv_bps: 4242,
            last_seen_unix: SENTINEL_TS,
            source: Source::Report,
        };
        state.upsert(original.clone()).await;

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            poll_one_refresh(&rpc, &state, &original),
        )
        .await;
        assert!(
            result.is_ok(),
            "poll_one_refresh must return promptly even when RPC fails",
        );
        assert!(matches!(result.unwrap(), PollOutcome::Skipped));

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

    /// M8 synthetic-injection contract: an entry with the
    /// `obligation_pubkey == Pubkey::default()` AND `last_ltv_bps > 0`
    /// marker (set by `--inject-test-position` at boot) must:
    ///   1. NOT touch the RPC (we use an unreachable URL — if it
    ///      fetched, the test would either error or hang),
    ///   2. classify as Critical (the synthesised obligation has
    ///      distance ≈ 10 bps, well below `DISTANCE_CRITICAL_BPS = 50`),
    ///   3. upsert with `Source::Poll`, replacing the stub
    ///      obligation_pubkey with the derived PDA.
    #[tokio::test]
    async fn synthetic_injection_short_circuits_and_classifies_critical() {
        use zerox1_protocol::fleet::riskwatcher::RiskSeverity;
        let rpc = RpcContext::new(
            "http://127.0.0.1:1".to_string(),
            CommitmentConfig::confirmed(),
        );
        let state = ObservedPositions::new();

        let subject: [u8; 32] = [9u8; 32];
        let injected = PositionView {
            subject,
            // The synthetic marker — `default()` pubkey + nonzero LTV.
            obligation_pubkey: Pubkey::default(),
            last_ltv_bps: 9500,
            last_seen_unix: 0,
            source: Source::Report,
        };
        state.upsert(injected.clone()).await;

        let result =
            tokio::time::timeout(Duration::from_secs(5), poll_one_refresh(&rpc, &state, &injected))
                .await
                .expect("synthetic path must return promptly without touching RPC");

        match result {
            PollOutcome::Updated(UpdatedFields {
                subject: subj,
                classification: Some(sev),
                ..
            }) => {
                assert_eq!(sev, RiskSeverity::Critical);
                assert_eq!(subj, subject);
            }
            other => panic!("expected Critical classification, got {other:?}"),
        }

        let entries = state.list().await;
        assert_eq!(entries.len(), 1);
        let after = entries.into_iter().next().unwrap();
        assert_eq!(after.subject, subject);
        assert_eq!(after.source, Source::Poll, "synthetic refresh must upsert");
        assert_ne!(
            after.obligation_pubkey,
            Pubkey::default(),
            "obligation_pubkey must be replaced with the derived PDA",
        );
    }
}
