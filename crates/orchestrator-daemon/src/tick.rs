//! The per-tick decision loop.
//!
//! Per tick (default 60s):
//!   1. Pull a fresh `FleetSnapshot` from the dashboard REST API
//!   2. Run the pure `allocator::decide` against it
//!   3. Pretty-print the recommendation
//!   4. Optionally — in execute mode — dispatch the action as a signed
//!      envelope through the embedded NodeHandle, subject to cooldown +
//!      stale-snapshot guards
//!   5. Append one JSONL line to the audit log
//!
//! Errors on per-tick I/O (dashboard fetch, audit write, envelope send)
//! are logged and the loop continues — a transient outage must not
//! kill the orchestrator.

use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use tokio::sync::Mutex;
use tracing::{info, warn};

use fleet_pm_stub::allocator::{decide, AllocatorAction, AllocatorConfig};
use fleet_pm_stub::allocator_runner::{
    action_to_envelope_spec, fetch_snapshot, print_action, ExecuteTargets, FleetSnapshot,
};
use zerox1_defi_runtime::identity::RoleIdentity;
use zerox1_node_enterprise::NodeHandle;

use crate::cooldown::CooldownTracker;
use crate::emit::{emit_envelope, EmitOutcome};
use crate::telemetry::AuditLog;

/// Optional execute-mode ingredients. When `Some`, the tick loop signs
/// and emits envelopes; when `None`, it only writes audit records.
pub struct ExecuteCtx {
    pub targets: ExecuteTargets,
    pub handle: NodeHandle,
    pub role_id: RoleIdentity,
    pub nonce: Arc<AtomicU64>,
    pub cooldown_secs: u64,
    pub wait_for_peer_secs: u64,
    /// Stale-snapshot guard: re-fetch before emit; reject if the
    /// re-fetched idle/deployed has moved by more than this factor.
    /// Default 1.10 (10% slack).
    pub stale_slack: f64,
}

/// Per-tick context. Cheap to clone; the execute-mode pieces live
/// inside `Option<ExecuteCtx>` so dry-run skips all of it cleanly.
pub struct TickCtx {
    pub api_base: String,
    pub cfg: AllocatorConfig,
    pub audit: Arc<AuditLog>,
    /// `"dry-run"` when `execute` is None, `"execute"` otherwise.
    pub mode: &'static str,
    pub execute: Option<ExecuteCtx>,
    /// Shared cooldown state; only the tick task consumes it but the
    /// mutex makes the borrow rules nicer across await points.
    pub cooldown: Arc<Mutex<CooldownTracker>>,
}

/// Long-running tick loop. Never returns under normal operation; on a
/// per-tick error it logs and continues — a transient dashboard outage
/// should not kill the orchestrator.
pub async fn run(ctx: Arc<TickCtx>, interval: Duration) -> Result<()> {
    info!(
        api_base = %ctx.api_base,
        interval_secs = interval.as_secs(),
        mode = ctx.mode,
        "orchestrator tick loop starting",
    );
    loop {
        match tick_once(&ctx).await {
            Ok(()) => {}
            Err(e) => warn!(?e, "tick failed — continuing"),
        }
        tokio::time::sleep(interval).await;
    }
}

async fn tick_once(ctx: &TickCtx) -> Result<()> {
    let snap = fetch_snapshot(&ctx.api_base).await?;
    let action = decide(&snap.strategies, snap.total_aum_usd, snap.idle_usd, &ctx.cfg);
    print_action(&action, &snap);

    let envelope_result = match (&ctx.execute, &action) {
        // Dry-run: just record what would have happened.
        (None, _) => String::new(),

        // Execute mode, NoAction: nothing to dispatch.
        (Some(_), AllocatorAction::NoAction { .. }) => String::new(),

        // Execute mode, deposit/withdraw: cooldown + stale-guard + emit.
        (Some(exec), action) => match dispatch(action, ctx, exec, &snap).await {
            Ok(s) => s,
            Err(e) => {
                warn!(?e, "dispatch failed");
                format!("failed:{e}")
            }
        },
    };

    // Resolve the TargetMode to a concrete TargetWeights for this tick
    // so the audit log records the EXACT target vector the picker saw.
    // For Static mode this is constant across ticks; for AprWeighted
    // it's recomputed from the snapshot's per-strategy APRs.
    let risk_free_bps = snap
        .strategies
        .iter()
        .find(|s| s.id == "stable_yield")
        .map(|s| s.nominal_apr_bps)
        .unwrap_or(0);
    let resolved_targets = ctx
        .cfg
        .target_weights
        .as_ref()
        .map(|m| m.resolve(&snap.strategies, &ctx.cfg, risk_free_bps));

    ctx.audit.append_with_result(
        ctx.mode,
        &snap,
        &action,
        &envelope_result,
        resolved_targets.as_ref(),
    )?;
    Ok(())
}

/// The execute-mode dispatch pipeline:
/// 1. Cooldown gate
/// 2. Action → EnvelopeSpec via shared library
/// 3. Stale-snapshot re-check
/// 4. Emit envelope
/// 5. Record the dispatch in the cooldown tracker
async fn dispatch(
    action: &AllocatorAction,
    ctx: &TickCtx,
    exec: &ExecuteCtx,
    snap: &FleetSnapshot,
) -> Result<String> {
    let strategy = match action {
        AllocatorAction::Deposit { strategy, .. }
        | AllocatorAction::Withdraw { strategy, .. } => strategy.clone(),
        AllocatorAction::NoAction { .. } => return Ok(String::new()),
    };

    let now = SystemTime::now();
    let cooldown_dur = Duration::from_secs(exec.cooldown_secs);
    {
        let cd = ctx.cooldown.lock().await;
        if cd.is_cooled_down(&strategy, now, cooldown_dur) {
            let elapsed = cd.seconds_since(&strategy, now).unwrap_or(0);
            info!(
                strategy,
                elapsed_secs = elapsed,
                cooldown_secs = exec.cooldown_secs,
                "skipping dispatch — strategy in cooldown",
            );
            return Ok(format!("skipped:cooldown_{elapsed}s"));
        }
    }

    let spec = match action_to_envelope_spec(action, &exec.targets)? {
        Some(s) => s,
        None => return Ok("skipped:no_dispatch".to_string()),
    };
    info!(label = spec.label, conv = %hex::encode(spec.conv_id), "envelope built");

    // Stale-snapshot guard: re-fetch immediately before signing and
    // verify the action is still credibly sized.
    let fresh = fetch_snapshot(&ctx.api_base).await?;
    if let Some(reason) = stale_snapshot_reason(action, snap, &fresh, exec.stale_slack) {
        info!(strategy, ?reason, "skipping dispatch — stale snapshot");
        return Ok(format!("skipped:stale_snapshot_{reason}"));
    }

    let outcome = emit_envelope(
        &spec,
        &exec.handle,
        &exec.role_id,
        &exec.nonce,
        exec.wait_for_peer_secs,
    )
    .await;

    // Record dispatch in the cooldown tracker regardless of outcome.
    // A failed send still consumes the per-strategy slot — better than
    // hammering a broken recipient every tick.
    {
        let mut cd = ctx.cooldown.lock().await;
        cd.record(&strategy, now);
    }

    let result = outcome.as_audit_string();
    if let EmitOutcome::Failed(_) = outcome {
        warn!(strategy, result = %result, "envelope dispatch failed");
    }
    Ok(result)
}

/// Returns `Some(reason)` if the re-fetched snapshot invalidates the
/// pending action (capital moved beyond `slack` factor). Otherwise None.
fn stale_snapshot_reason(
    action: &AllocatorAction,
    _orig: &FleetSnapshot,
    fresh: &FleetSnapshot,
    slack: f64,
) -> Option<String> {
    match action {
        AllocatorAction::NoAction { .. } => None,
        AllocatorAction::Deposit { amount_usd, .. } => {
            let limit = fresh.idle_usd * slack;
            if *amount_usd > limit {
                Some(format!(
                    "deposit_{amount_usd:.2}_exceeds_idle_{:.2}",
                    fresh.idle_usd
                ))
            } else {
                None
            }
        }
        AllocatorAction::Withdraw {
            strategy,
            amount_usd,
            ..
        } => {
            let deployed = fresh
                .strategies
                .iter()
                .find(|s| &s.id == strategy)
                .map(|s| s.deployed_usd)
                .unwrap_or(0.0);
            let limit = deployed * slack;
            if *amount_usd > limit {
                Some(format!(
                    "withdraw_{amount_usd:.2}_exceeds_deployed_{deployed:.2}"
                ))
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_pm_stub::allocator::StrategyRate;

    fn snap(stable_deployed: f64, idle: f64) -> FleetSnapshot {
        FleetSnapshot {
            total_aum_usd: stable_deployed + idle,
            idle_usd: idle,
            strategies: vec![StrategyRate {
                id: "stable_yield".into(),
                deployed_usd: stable_deployed,
                nominal_apr_bps: 700,
            }],
        }
    }

    #[test]
    fn deposit_within_slack_passes() {
        let action = AllocatorAction::Deposit {
            strategy: "stable_yield".into(),
            amount_usd: 100.0,
            reason: "".into(),
        };
        // fresh idle dropped to $95 but slack 1.10 allows up to $104.50
        let s = snap(0.0, 95.0);
        assert!(stale_snapshot_reason(&action, &s, &s, 1.10).is_none());
    }

    #[test]
    fn deposit_beyond_slack_rejected() {
        let action = AllocatorAction::Deposit {
            strategy: "stable_yield".into(),
            amount_usd: 100.0,
            reason: "".into(),
        };
        // fresh idle dropped to $50 — way under the requested $100
        let s = snap(0.0, 50.0);
        let r = stale_snapshot_reason(&action, &s, &s, 1.10);
        assert!(r.is_some());
        assert!(r.unwrap().contains("deposit_100"));
    }

    #[test]
    fn withdraw_within_slack_passes() {
        let action = AllocatorAction::Withdraw {
            strategy: "stable_yield".into(),
            amount_usd: 50.0,
            reason: "".into(),
        };
        let s = snap(48.0, 0.0); // 48 deployed, 50 × 1.10 = 55 allowed
        assert!(stale_snapshot_reason(&action, &s, &s, 1.10).is_none());
    }

    #[test]
    fn withdraw_beyond_slack_rejected() {
        let action = AllocatorAction::Withdraw {
            strategy: "stable_yield".into(),
            amount_usd: 100.0,
            reason: "".into(),
        };
        let s = snap(20.0, 0.0); // only $20 deployed, can't withdraw $100
        let r = stale_snapshot_reason(&action, &s, &s, 1.10);
        assert!(r.is_some());
        assert!(r.unwrap().contains("withdraw_100"));
    }

    #[test]
    fn no_action_never_stale() {
        let action = AllocatorAction::NoAction {
            reason: "".into(),
        };
        let s = snap(0.0, 0.0);
        assert!(stale_snapshot_reason(&action, &s, &s, 1.10).is_none());
    }
}
