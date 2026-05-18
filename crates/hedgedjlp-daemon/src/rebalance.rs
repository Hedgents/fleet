//! Periodic rebalancer + borrow-rate watch (M9).
//!
//! On every tick:
//!   1. Read live pool state (`jlp_hedge::read_pool_state`).
//!   2. Compute drift between current and target net delta. If
//!      `|drift| > MAX_DELTA_DRIFT_BPS` emit `EscalateRisk(Notice,
//!      DeltaDrift)` and queue a resize (resize itself is M11+).
//!   3. For each custody on the pool, read its borrow-rate state and
//!      compare against the active assignment's `max_borrow_rate_bps`.
//!      If exceeded, emit `EscalateRisk(Warning, PerpFundingSpike)` and
//!      set `paused_until_unix = now + 1h` so subsequent ticks no-op.
//!
//! v0 tracks one active position via `Mutex<Option<ActivePosition>>` —
//! there's no full position book yet (M11+). Without an active position
//! the tick logs and no-ops.
//!
//! The risk taxonomy in `zerox1_protocol::fleet::riskwatcher::RiskKind`
//! is read-only for hedgedjlp's purposes (the protocol crate lives in
//! `node-enterprise`). Mapping:
//!   - rebalance drift             → `RiskKind::DeltaDrift`
//!   - borrow rate exceeded        → `RiskKind::PerpFundingSpike`
//!     (closest existing variant — Jupiter Perps borrow rates are the
//!      direct analog of perp funding under their model; a dedicated
//!      `BorrowRateExceeded` variant lands when the protocol crate
//!      next opens for additions.)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use solana_sdk::pubkey::Pubkey;
use tokio::time::interval;
use tracing::{info, warn};

use zerox1_defi_runtime::identity::RoleIdentity;
use zerox1_defi_runtime::rpc::RpcContext;
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::envelope::Envelope;
use zerox1_protocol::fleet::riskwatcher::{EscalateRisk, RiskKind, RiskSeverity};
use zerox1_protocol::message::MsgType;

use crate::caps::MAX_DELTA_DRIFT_BPS;
use crate::jlp_hedge::read_pool_state;

/// One in-flight position the rebalancer is responsible for. v0 stores
/// a single position globally; a real position book lands in M11+.
#[derive(Debug, Clone)]
pub struct ActivePosition {
    /// Conversation id of the original Assign — for telemetry.
    pub conv: [u8; 16],
    /// Daemon's JLP holdings (raw lamports).
    pub our_jlp_lamports: u64,
    /// JLP lamports actually acquired at open — used by the unwind path
    /// to compute how many JLP to burn for `payload.jlp_lamports =
    /// u64::MAX` (full unwind). v0 sets this equal to `our_jlp_lamports`
    /// at record time; later resize work may decouple them.
    pub jlp_acquired_lamports: u64,
    /// Target net long exposure ratio (bps of total). Same semantics as
    /// `AssignHedgedJlp.target_delta_bps`.
    pub target_delta_bps: i16,
    /// Cap on per-custody borrow rate the daemon will tolerate.
    pub max_borrow_rate_bps: u16,
    /// Custody pubkeys to read on each tick. Set when the assignment
    /// is recorded; empty list = no rebalance possible (skip).
    pub custody_pubkeys: Vec<Pubkey>,
    /// Notional USDC sized into hedge legs at open (M10 telemetry). v0
    /// recorders set this from the executor's `hedge_notional_usdc`
    /// return; rebalances do not currently mutate it (resize lands in
    /// M11+).
    pub hedge_notional_usdc: u64,
    /// Per-asset open hedge positions, recorded at open by `hedge.rs`
    /// and consumed by `unwind.rs` to build close-request ixns.
    /// Each entry is `(asset_label, position_pubkey, open_counter)` —
    /// the counter is the `unix_seconds + i` value used by `hedge.rs`
    /// at open time. The unwind path reuses this counter when deriving
    /// the close-request `PositionRequest` PDA so the close PDA matches
    /// the open PDA (audit-fix C2). Empty list means no real open
    /// positions are tracked — withdraw returns a zero-Report.
    pub open_positions: Vec<(String, Pubkey, u64)>,
}

/// Shared rebalancer state. Wrapped in an `Arc` so the dispatch path
/// can write to it (record / clear) and the rebalance loop can read.
pub struct RebalanceState {
    /// Current active position, if any.
    pub active: Mutex<Option<ActivePosition>>,
    /// Rebalances paused until this unix-seconds value (set after
    /// `BorrowRateExceeded` to throttle further work for ~1h).
    pub paused_until_unix: Mutex<u64>,
}

impl RebalanceState {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(None),
            paused_until_unix: Mutex::new(0),
        }
    }

    /// Non-blocking clone of the current active position, if any.
    /// Used by telemetry (M10) which wants a read-only snapshot per
    /// tick without holding the inner mutex across awaits.
    pub fn snapshot_active_position(&self) -> Option<ActivePosition> {
        self.active.lock().expect("active poisoned").clone()
    }

    /// Record an active position. Overwrites any existing one — v0 is
    /// single-position. The unwind path calls `clear_active_position`
    /// when the position fully closes.
    pub fn set_active_position(&self, pos: ActivePosition) {
        *self.active.lock().expect("active poisoned") = Some(pos);
    }

    /// Clear the active position. Called by `unwind::run_or_simulate`
    /// after submitting close requests + JLP burn; the rebalancer +
    /// telemetry then no-op (no active position) until the next
    /// AssignHedgedJlp records a fresh one.
    pub fn clear_active_position(&self) {
        *self.active.lock().expect("active poisoned") = None;
    }
}

impl Default for RebalanceState {
    fn default() -> Self {
        Self::new()
    }
}

/// Pause window after a borrow-rate exceedance — 1 hour.
pub const BORROW_PAUSE_SECS: u64 = 3_600;

/// Rebalancer entry point. Spawned by `main` if
/// `--rebalance-interval-secs > 0`.
///
/// `resize_ctx` (`Option<Arc<crate::resize::ResizeCtx>>`) gates the
/// resize action: when present and drift exceeds `MAX_DELTA_DRIFT_BPS`,
/// the rebalancer calls `resize::run_resize` to queue an operator-
/// approval-gated resize plan. When `None`, drift detection still emits
/// the `Escalate(DeltaDrift)` envelope (for telemetry / dashboard) but
/// no resize work is queued — used by tests + the historical M9 shape.
pub async fn run(
    rpc: Arc<RpcContext>,
    handle: NodeHandle,
    role: RoleIdentity,
    nonce: Arc<AtomicU64>,
    state: Arc<RebalanceState>,
    resize_ctx: Option<Arc<crate::resize::ResizeCtx>>,
    interval_dur: Duration,
) {
    info!(secs = interval_dur.as_secs(), "rebalancer starting");

    let mut tick = interval(interval_dur);
    // Skip the first immediate tick — we want first work after one
    // interval, not at startup.
    tick.tick().await;

    loop {
        tick.tick().await;
        if let Err(e) = tick_once(&rpc, &handle, &role, &nonce, &state, resize_ctx.as_ref()).await {
            warn!(?e, "rebalance tick errored — daemon stays alive");
        }
    }
}

/// One rebalance tick. Public for testability — most logic is in
/// helpers that don't touch RPC so unit tests can drive them directly.
pub async fn tick_once(
    rpc: &Arc<RpcContext>,
    handle: &NodeHandle,
    role: &RoleIdentity,
    nonce: &Arc<AtomicU64>,
    state: &Arc<RebalanceState>,
    resize_ctx: Option<&Arc<crate::resize::ResizeCtx>>,
) -> anyhow::Result<()> {
    let now = now_unix();

    // Honor pause window from a recent borrow-rate exceedance.
    let paused = *state.paused_until_unix.lock().expect("paused poisoned");
    if now < paused {
        info!(
            paused_until_unix = paused,
            "rebalance tick skipped — paused after recent BorrowRateExceeded"
        );
        return Ok(());
    }

    // Snapshot active position (clone so the lock isn't held across awaits).
    let active = state.active.lock().expect("active poisoned").clone();
    let position = match active {
        Some(p) => p,
        None => {
            info!("rebalance tick skipped — no active position");
            return Ok(());
        }
    };

    if position.custody_pubkeys.is_empty() {
        info!(
            ?position.conv,
            "rebalance tick: active position has no custody list — skipping read_pool_state"
        );
        return Ok(());
    }

    info!(
        ?position.conv,
        custody_count = position.custody_pubkeys.len(),
        "rebalance tick: calling read_pool_state"
    );

    match read_pool_state(rpc, position.our_jlp_lamports, &position.custody_pubkeys).await {
        Ok((delta, supply)) => {
            info!(
                ?position.conv,
                total_usd = delta.total_usd,
                long_bps = delta.long_exposure_bps,
                jlp_supply = supply,
                "read_pool_state ok"
            );

            // Drift detection: current_long minus target_long, expressed
            // as bps of total. Reuses the corrected-formula math.
            let drift_bps = compute_drift_bps(&delta, position.target_delta_bps);
            if drift_bps.unsigned_abs() > MAX_DELTA_DRIFT_BPS as u32 {
                warn!(
                    ?position.conv,
                    drift_bps,
                    cap_bps = MAX_DELTA_DRIFT_BPS,
                    "delta drift exceeds cap — emitting Escalate(DeltaDrift)"
                );
                let _ = emit_escalate(
                    handle,
                    role,
                    nonce,
                    RiskSeverity::Notice,
                    RiskKind::DeltaDrift,
                    position.conv,
                    drift_bps as i64,
                )
                .await;
                // Resize action. Closes the M9 gap: previously this
                // branch logged + escalated but did nothing on chain.
                // `run_resize` computes the per-asset delta-to-open
                // (existing shorts subtracted) and queues a plan for
                // operator approval — never re-opens legs already on
                // chain. When the resize context is `None` (older
                // wiring / test harness), drift detection still emits
                // the Escalate but no resize work happens.
                match resize_ctx {
                    Some(rctx) => {
                        match crate::resize::run_resize(rctx, &position, &delta).await {
                            Ok(outcome) => {
                                info!(
                                    ?position.conv,
                                    queued = outcome.queued.len(),
                                    skipped = outcome.skipped.len(),
                                    queued_to_approval = outcome.queued_to_approval,
                                    ?outcome.cap_hit_usdc,
                                    "rebalance resize evaluated"
                                );
                            }
                            Err(e) => {
                                warn!(?e, ?position.conv, "run_resize errored — drift unhandled this tick");
                            }
                        }
                    }
                    None => {
                        info!(
                            ?position.conv,
                            "resize_ctx not wired — drift escalated but no resize plan queued"
                        );
                    }
                }
            } else {
                info!(?position.conv, drift_bps, "delta drift within cap");
            }

            // Borrow-rate watch: per-custody. The accessor returns
            // `Option<u16>` until the offset is verified — `None`
            // means "skip the borrow check this tick".
            let mut max_observed: Option<u16> = None;
            for cp in &position.custody_pubkeys {
                let data = match rpc.client.get_account_data(cp).await {
                    Ok(d) => d,
                    Err(e) => {
                        warn!(?e, custody = %cp, "borrow-rate read failed for custody");
                        continue;
                    }
                };
                if let Some(bps) =
                    zerox1_defi_protocols::protocols::jlp::decode_custody_borrow_rate_bps(&data)
                {
                    max_observed = Some(max_observed.map(|m| m.max(bps)).unwrap_or(bps));
                }
            }
            if let Some(observed) = max_observed {
                if observed > position.max_borrow_rate_bps {
                    warn!(
                        ?position.conv,
                        observed_bps = observed,
                        cap_bps = position.max_borrow_rate_bps,
                        "borrow rate exceeds assignment cap — pausing rebalances"
                    );
                    let _ = emit_escalate(
                        handle,
                        role,
                        nonce,
                        RiskSeverity::Warning,
                        RiskKind::PerpFundingSpike,
                        position.conv,
                        observed as i64,
                    )
                    .await;
                    *state.paused_until_unix.lock().expect("paused poisoned") =
                        now.saturating_add(BORROW_PAUSE_SECS);
                } else {
                    info!(
                        ?position.conv,
                        observed_bps = observed,
                        cap_bps = position.max_borrow_rate_bps,
                        "borrow rate within cap"
                    );
                }
            } else {
                info!(
                    ?position.conv,
                    "borrow rate read returned None — borrow watch skipped (offset pending verify)"
                );
            }
        }
        Err(e) => {
            warn!(
                ?e,
                "read_pool_state failed — likely devnet (Jupiter Perps mainnet-only). Tick continues."
            );
        }
    }

    Ok(())
}

/// Drift = current_long_bps - target_net_long_bps. Both sides expressed
/// as bps of `total_usd`. Returns a signed i32 in range [-20_000, +20_000].
pub fn compute_drift_bps(delta: &crate::delta::PortfolioDelta, target_delta_bps: i16) -> i32 {
    if delta.total_usd == 0 {
        return 0;
    }
    let total = delta.total_usd as i128;
    let current_long = (delta.sol_usd as i128)
        .saturating_add(delta.eth_usd as i128)
        .saturating_add(delta.btc_usd as i128);
    let current_long_bps = (current_long.saturating_mul(10_000) / total) as i32;
    let target_bps = target_delta_bps as i32;
    current_long_bps - target_bps
}

/// Emit an EscalateRisk envelope as a broadcast on the mesh. Re-uses
/// the conv id of the active assignment for correlation.
async fn emit_escalate(
    handle: &NodeHandle,
    role: &RoleIdentity,
    nonce: &Arc<AtomicU64>,
    severity: RiskSeverity,
    kind: RiskKind,
    conv: [u8; 16],
    measurement: i64,
) -> anyhow::Result<()> {
    use anyhow::Context;

    let signing_key = ed25519_dalek::SigningKey::from_bytes(role.signing_key_bytes());
    let sender = signing_key.verifying_key().to_bytes();
    let now = now_unix();

    let payload = EscalateRisk {
        severity,
        kind,
        subject: [0u8; 32],
        measurement,
        raised_at_unix: now,
    };
    let mut payload_bytes = Vec::new();
    ciborium::ser::into_writer(&payload, &mut payload_bytes).context("serialize EscalateRisk")?;

    let n = nonce.fetch_add(1, Ordering::Relaxed);
    let env = Envelope::build(
        MsgType::Escalate,
        sender,
        zerox1_protocol::envelope::BROADCAST_RECIPIENT,
        now,
        n,
        conv,
        payload_bytes,
        &signing_key,
    );
    handle.send(env).await.context("send EscalateRisk")?;
    info!(?conv, ?severity, ?kind, "EscalateRisk emitted");
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::PortfolioDelta;

    fn delta_balanced() -> PortfolioDelta {
        PortfolioDelta {
            sol_usd: 50_000_000,
            eth_usd: 30_000_000,
            btc_usd: 20_000_000,
            stable_usd: 100_000_000,
            total_usd: 200_000_000,
            long_exposure_bps: 5_000,
        }
    }

    #[test]
    fn drift_zero_when_target_matches_current_long_bps() {
        // current_long = 100M of 200M total = 5_000 bps. target = 5_000.
        let d = delta_balanced();
        let drift = compute_drift_bps(&d, 5_000);
        assert_eq!(drift, 0);
    }

    #[test]
    fn drift_positive_when_long_exceeds_target() {
        // target=0 → drift = current_long_bps - 0 = 5_000.
        let d = delta_balanced();
        let drift = compute_drift_bps(&d, 0);
        assert_eq!(drift, 5_000);
    }

    #[test]
    fn drift_negative_when_target_exceeds_current() {
        // target=8_000, current=5_000 → drift = -3_000.
        let d = delta_balanced();
        let drift = compute_drift_bps(&d, 8_000);
        assert_eq!(drift, -3_000);
    }

    #[test]
    fn drift_zero_total_usd_returns_zero() {
        let mut d = delta_balanced();
        d.total_usd = 0;
        d.sol_usd = 0;
        d.eth_usd = 0;
        d.btc_usd = 0;
        assert_eq!(compute_drift_bps(&d, 5_000), 0);
    }

    #[test]
    fn rebalance_state_starts_inactive() {
        let s = RebalanceState::new();
        assert!(s.active.lock().unwrap().is_none());
        assert_eq!(*s.paused_until_unix.lock().unwrap(), 0);
    }

    #[test]
    fn rebalance_state_record_then_clear() {
        let s = RebalanceState::new();
        let pos = ActivePosition {
            conv: [1u8; 16],
            our_jlp_lamports: 1_000_000,
            jlp_acquired_lamports: 1_000_000,
            target_delta_bps: 0,
            max_borrow_rate_bps: 3_000,
            custody_pubkeys: vec![],
            hedge_notional_usdc: 0,
            open_positions: vec![],
        };
        s.set_active_position(pos.clone());
        assert!(s.active.lock().unwrap().is_some());
        s.clear_active_position();
        assert!(s.active.lock().unwrap().is_none());
    }

    #[test]
    fn rebalance_state_set_and_clear_helpers_round_trip() {
        let s = RebalanceState::new();
        let pos = ActivePosition {
            conv: [9u8; 16],
            our_jlp_lamports: 7,
            jlp_acquired_lamports: 7,
            target_delta_bps: -200,
            max_borrow_rate_bps: 500,
            custody_pubkeys: vec![Pubkey::new_unique()],
            hedge_notional_usdc: 100,
            open_positions: vec![("SOL".to_string(), Pubkey::new_unique(), 12345)],
        };
        s.set_active_position(pos.clone());
        let snap = s.snapshot_active_position().expect("snapshot");
        assert_eq!(snap.conv, pos.conv);
        assert_eq!(snap.open_positions.len(), 1);
        // Audit-fix C2: counter is preserved through the round-trip.
        assert_eq!(snap.open_positions[0].2, 12345);
        s.clear_active_position();
        assert!(s.snapshot_active_position().is_none());
    }

    #[test]
    fn drift_above_cap_triggers_logically() {
        // |drift| > MAX_DELTA_DRIFT_BPS (1000): construct a case.
        // target=0, current_long_bps=2000 → drift=+2000 (above cap).
        let mut d = delta_balanced();
        d.sol_usd = 40_000_000;
        d.eth_usd = 0;
        d.btc_usd = 0;
        d.stable_usd = 160_000_000;
        d.total_usd = 200_000_000;
        let drift = compute_drift_bps(&d, 0);
        assert_eq!(drift, 2_000);
        assert!(drift.unsigned_abs() > MAX_DELTA_DRIFT_BPS as u32);
    }

    #[test]
    fn borrow_pause_window_is_one_hour() {
        // Pin the pause window — operators expect ~1h throttle.
        assert_eq!(BORROW_PAUSE_SECS, 3_600);
    }

    #[test]
    fn borrow_rate_watch_decodes_value_for_full_size_data() {
        // Audit fix 7: the accessor now returns Some(bps) for any
        // slice large enough to cover the FundingRateState offset.
        // The actual numeric value comes from `hourlyFundingDbps/10`.
        let data = vec![0u8; 1200];
        let r = zerox1_defi_protocols::protocols::jlp::decode_custody_borrow_rate_bps(&data);
        assert_eq!(r, Some(0), "zero-fill data → zero bps (no spike)");

        // Short slice → None (skip the tick).
        let short = vec![0u8; 100];
        let r = zerox1_defi_protocols::protocols::jlp::decode_custody_borrow_rate_bps(&short);
        assert!(
            r.is_none(),
            "short slice returns None — borrow watch skipped"
        );
    }
}
