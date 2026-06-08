//! Per-strategy harvest loop.
//!
//! Runs on a slow cadence alongside the 60s allocator tick. Three jobs:
//!   1. **stable_yield** — no-op. Auto-compounds inside Kamino as the
//!      kToken share value grows.
//!   2. **multiply** — when collateral appreciation has dragged LTV
//!      below target by more than `ltv_drift_bps`, emit an
//!      `AssignMultiply { target_ltv_bps, usdc_lamports: 0 }` to
//!      re-borrow against the appreciated collateral and restore the
//!      target leverage. This is the actual compounding action — without
//!      it, leverage decays as jitoSOL grows and the strategy's yield
//!      drops over time.
//!   3. **hedgedjlp** — log unrealised perp PnL but DO NOT act. Jupiter
//!      Perps accrues funding + mark-to-market inside open positions
//!      (the dashboard's "Realtime perp PnL" number), and realising it
//!      requires a partial-close instruction the hedgedjlp daemon
//!      doesn't expose yet. Above the configured threshold we log a
//!      recommendation so the operator knows to harvest manually.
//!
//! Decisions are written to a separate JSONL audit log
//! (`harvest-audit.jsonl`) so the existing allocator audit schema stays
//! unchanged.

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{info, warn};

use fleet_pm_stub::allocator_runner::{
    build_hedgedjlp_full_withdraw_spec, build_multiply_releverage_spec, now_unix, ExecuteTargets,
};
use zerox1_defi_runtime::identity::RoleIdentity;
use zerox1_node_enterprise::NodeHandle;

use crate::cooldown::CooldownTracker;
use crate::emit::{emit_envelope, EmitOutcome};

/// Cooldown key used by the harvest loop's re-leverage path. Distinct
/// from the allocator tick's `"multiply"` key so a harvest re-leverage
/// doesn't suppress an allocator deposit/withdraw (or vice versa).
pub const HARVEST_MULTIPLY_KEY: &str = "harvest_multiply";

/// Cooldown key used by the harvest loop's hedgedjlp PnL realize path.
/// Distinct from the allocator tick's `"hedgedjlp"` key.
pub const HARVEST_HEDGEDJLP_KEY: &str = "harvest_hedgedjlp";

/// Default cadence: every 6 hours. Slow on purpose — multiply LTV drift
/// is hours-scale and hedgedjlp PnL is even slower.
pub const DEFAULT_INTERVAL_SECS: u64 = 6 * 60 * 60;

/// Default re-leverage trigger: act when current LTV is at least 150bps
/// below target. Tight enough to actually compound, loose enough to
/// ignore intraday oracle wobble.
pub const DEFAULT_LTV_DRIFT_BPS: i32 = 150;

/// Default hedgedjlp PnL log threshold ($USD). Scaled to current
/// sub-$100 collateral — at larger fleet sizes this should grow.
pub const DEFAULT_PNL_THRESHOLD_USD: f64 = 5.0;

/// Default target LTV for multiply re-leverage. Matches the existing
/// deposit path's hardcoded target. See
/// `fleet_pm_stub::allocator_runner::action_to_envelope_spec`.
pub const DEFAULT_TARGET_LTV_BPS: u16 = 6000;

/// Optional execute-mode ingredients. `None` for dry-run; the loop
/// still observes and audits but skips dispatch.
pub struct HarvestExecuteCtx {
    pub targets: ExecuteTargets,
    pub handle: NodeHandle,
    pub role_id: RoleIdentity,
    pub nonce: Arc<AtomicU64>,
    pub wait_for_peer_secs: u64,
    /// Shared with the allocator tick. The harvest loop uses a separate
    /// cooldown key (`HARVEST_MULTIPLY_KEY`) so its dispatches don't
    /// suppress allocator dispatches and vice versa.
    pub cooldown: Arc<Mutex<CooldownTracker>>,
    pub cooldown_secs: u64,
}

/// Per-tick context.
pub struct HarvestCtx {
    pub api_base: String,
    pub audit_path: PathBuf,
    pub mode: &'static str,
    pub execute: Option<HarvestExecuteCtx>,
    pub ltv_drift_bps: i32,
    pub pnl_threshold_usd: f64,
    pub target_ltv_bps: u16,
}

/// Multiply position state extracted from `GET /positions`.
#[derive(Debug, Clone, Deserialize)]
struct MultiplyPositionView {
    #[serde(default)]
    ltv_bps: u16,
    #[serde(default)]
    deposited_usd: f64,
    #[serde(default)]
    borrowed_usd: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct PositionsView {
    #[serde(default)]
    multiply: Option<MultiplyPositionView>,
}

/// Per-strategy fields the harvest loop reads from `/strategies`. Kept
/// minimal — we only need the realtime PnL and the lifetime-earned
/// figure for logging.
#[derive(Debug, Clone, Deserialize)]
struct StrategyHarvestView {
    id: String,
    #[serde(default)]
    realtime_protocol_pnl_usdc: Option<f64>,
    #[serde(default)]
    lifetime_earned_usdc: Option<f64>,
    #[serde(default)]
    deployed_usdc: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct StrategiesView {
    strategies: Vec<StrategyHarvestView>,
}

/// One harvest tick's outcome. Serialised to the harvest JSONL audit.
#[derive(Debug, Clone, Serialize)]
struct HarvestRecord<'a> {
    ts_unix: u64,
    mode: &'a str,
    multiply: MultiplyOutcome,
    hedgedjlp: HedgedjlpOutcome,
    stable_yield: StableYieldOutcome,
}

#[derive(Debug, Clone, Serialize)]
struct MultiplyOutcome {
    current_ltv_bps: u16,
    target_ltv_bps: u16,
    drift_bps: i32,
    deployed_usd: f64,
    lifetime_earned_usd: Option<f64>,
    decision: &'static str,
    envelope_result: String,
}

#[derive(Debug, Clone, Serialize)]
struct HedgedjlpOutcome {
    realtime_pnl_usd: Option<f64>,
    threshold_usd: f64,
    deployed_usd: f64,
    lifetime_earned_usd: Option<f64>,
    decision: &'static str,
    envelope_result: String,
}

#[derive(Debug, Clone, Serialize)]
struct StableYieldOutcome {
    deployed_usd: f64,
    lifetime_earned_usd: Option<f64>,
    decision: &'static str,
}

/// Pure decision: should the harvest loop emit a re-leverage AssignMultiply?
/// `current_ltv_bps` may legitimately be 0 (no position yet) — we treat
/// that as "nothing to compound" and skip.
pub fn should_releverage_multiply(
    current_ltv_bps: u16,
    target_ltv_bps: u16,
    drift_threshold_bps: i32,
) -> bool {
    if current_ltv_bps == 0 || target_ltv_bps == 0 {
        return false;
    }
    let drift = target_ltv_bps as i32 - current_ltv_bps as i32;
    drift > drift_threshold_bps
}

/// Pure decision: should we surface a "manual harvest recommended"
/// log line for hedgedjlp? `None` PnL (no open shorts / no data) → false.
pub fn should_flag_hedgedjlp_pnl(pnl_usd: Option<f64>, threshold_usd: f64) -> bool {
    matches!(pnl_usd, Some(v) if v > threshold_usd)
}

/// Long-running harvest loop. Never returns under normal operation; per-tick
/// errors are logged and the loop continues.
pub async fn run(ctx: Arc<HarvestCtx>, interval: Duration) -> Result<()> {
    info!(
        api_base = %ctx.api_base,
        interval_secs = interval.as_secs(),
        ltv_drift_bps = ctx.ltv_drift_bps,
        pnl_threshold_usd = ctx.pnl_threshold_usd,
        target_ltv_bps = ctx.target_ltv_bps,
        mode = ctx.mode,
        "harvest loop starting",
    );
    loop {
        match tick_once(&ctx).await {
            Ok(()) => {}
            Err(e) => warn!(?e, "harvest tick failed — continuing"),
        }
        tokio::time::sleep(interval).await;
    }
}

async fn tick_once(ctx: &HarvestCtx) -> Result<()> {
    let positions = fetch_positions(&ctx.api_base).await?;
    let strategies = fetch_strategies(&ctx.api_base).await?;

    let strat = |id: &str| strategies.strategies.iter().find(|s| s.id == id).cloned();

    let multiply_view = positions.multiply.clone().unwrap_or(MultiplyPositionView {
        ltv_bps: 0,
        deposited_usd: 0.0,
        borrowed_usd: 0.0,
    });
    let multiply_strat = strat("multiply");
    let hedgedjlp_strat = strat("hedgedjlp");
    let stable_strat = strat("stable_yield");

    // ── multiply: re-leverage decision ────────────────────────────────
    // Surface drift_bps = 0 when there is no position so the audit row
    // doesn't read "6000bps drifted" for a non-existent position.
    let drift_bps = if multiply_view.ltv_bps == 0 {
        0
    } else {
        ctx.target_ltv_bps as i32 - multiply_view.ltv_bps as i32
    };
    let should_releverage = should_releverage_multiply(
        multiply_view.ltv_bps,
        ctx.target_ltv_bps,
        ctx.ltv_drift_bps,
    );
    let (multiply_decision, multiply_envelope_result) = if should_releverage {
        dispatch_multiply_releverage(ctx).await
    } else {
        let reason = if multiply_view.ltv_bps == 0 {
            "no_position"
        } else if drift_bps <= ctx.ltv_drift_bps {
            "within_drift_band"
        } else {
            "skipped"
        };
        (reason, String::new())
    };

    // ── hedgedjlp: PnL realize via full-unwind ────────────────────────
    // When PnL crosses the threshold AND we're in execute mode AND the
    // hedgedjlp daemon has opted into full-withdraw auto-accept, we send
    // a WithdrawHedgedJlp { jlp_lamports: u64::MAX } to close the
    // position. The accumulated funding + mark-to-market PnL settles to
    // USDC in the wallet; the 60s allocator tick redeploys the freed
    // capital back into a fresh hedgedjlp position. Net effect: PnL
    // realized and the position immediately reopened, minus round-trip
    // close/open fees.
    let realtime_pnl = hedgedjlp_strat.as_ref().and_then(|s| s.realtime_protocol_pnl_usdc);
    let (hedgedjlp_decision, hedgedjlp_envelope_result) =
        if should_flag_hedgedjlp_pnl(realtime_pnl, ctx.pnl_threshold_usd) {
            warn!(
                pnl_usd = realtime_pnl.unwrap_or(0.0),
                threshold_usd = ctx.pnl_threshold_usd,
                "hedgedjlp unrealised perp PnL above threshold — initiating realize-via-unwind"
            );
            dispatch_hedgedjlp_realize(ctx).await
        } else {
            ("below_threshold", String::new())
        };

    // ── stable_yield: observe only ────────────────────────────────────
    let stable_decision = "auto_compounds_in_protocol";

    let multiply_lifetime = multiply_strat.as_ref().and_then(|s| s.lifetime_earned_usdc);
    let hedgedjlp_lifetime = hedgedjlp_strat.as_ref().and_then(|s| s.lifetime_earned_usdc);
    let stable_lifetime = stable_strat.as_ref().and_then(|s| s.lifetime_earned_usdc);
    let multiply_deployed = multiply_strat
        .as_ref()
        .map(|s| s.deployed_usdc)
        .unwrap_or_else(|| multiply_view.deposited_usd - multiply_view.borrowed_usd);
    let hedgedjlp_deployed = hedgedjlp_strat.as_ref().map(|s| s.deployed_usdc).unwrap_or(0.0);
    let stable_deployed = stable_strat.as_ref().map(|s| s.deployed_usdc).unwrap_or(0.0);

    info!(
        multiply_ltv_bps = multiply_view.ltv_bps,
        multiply_decision,
        multiply_envelope = multiply_envelope_result.as_str(),
        hedgedjlp_pnl_usd = realtime_pnl.unwrap_or(0.0),
        hedgedjlp_decision,
        stable_decision,
        "harvest tick"
    );

    let rec = HarvestRecord {
        ts_unix: now_unix(),
        mode: ctx.mode,
        multiply: MultiplyOutcome {
            current_ltv_bps: multiply_view.ltv_bps,
            target_ltv_bps: ctx.target_ltv_bps,
            drift_bps,
            deployed_usd: multiply_deployed,
            lifetime_earned_usd: multiply_lifetime,
            decision: multiply_decision,
            envelope_result: multiply_envelope_result,
        },
        hedgedjlp: HedgedjlpOutcome {
            realtime_pnl_usd: realtime_pnl,
            threshold_usd: ctx.pnl_threshold_usd,
            deployed_usd: hedgedjlp_deployed,
            lifetime_earned_usd: hedgedjlp_lifetime,
            decision: hedgedjlp_decision,
            envelope_result: hedgedjlp_envelope_result,
        },
        stable_yield: StableYieldOutcome {
            deployed_usd: stable_deployed,
            lifetime_earned_usd: stable_lifetime,
            decision: stable_decision,
        },
    };
    append_harvest_audit(&ctx.audit_path, &rec)?;
    Ok(())
}

async fn dispatch_multiply_releverage(ctx: &HarvestCtx) -> (&'static str, String) {
    let Some(exec) = ctx.execute.as_ref() else {
        return ("releverage_dryrun", String::new());
    };

    // Cooldown gate — separate key so allocator's "multiply" key doesn't
    // collide with the harvest re-leverage.
    let now = std::time::SystemTime::now();
    let cooldown_dur = Duration::from_secs(exec.cooldown_secs);
    {
        let cd = exec.cooldown.lock().await;
        if cd.is_cooled_down(HARVEST_MULTIPLY_KEY, now, cooldown_dur) {
            let elapsed = cd.seconds_since(HARVEST_MULTIPLY_KEY, now).unwrap_or(0);
            info!(
                elapsed_secs = elapsed,
                cooldown_secs = exec.cooldown_secs,
                "harvest re-leverage skipped — in cooldown",
            );
            return ("releverage_cooldown", format!("skipped:cooldown_{elapsed}s"));
        }
    }

    let spec = match build_multiply_releverage_spec(&exec.targets, ctx.target_ltv_bps) {
        Ok(Some(s)) => s,
        Ok(None) => {
            info!("harvest re-leverage skipped — no multiply target configured");
            return ("releverage_no_target", String::new());
        }
        Err(e) => {
            warn!(?e, "harvest re-leverage spec build failed");
            return ("releverage_error", format!("failed:{e}"));
        }
    };
    info!(label = spec.label, conv = %hex::encode(spec.conv_id), "harvest envelope built");

    let outcome = emit_envelope(
        &spec,
        &exec.handle,
        &exec.role_id,
        &exec.nonce,
        exec.wait_for_peer_secs,
    )
    .await;

    {
        let mut cd = exec.cooldown.lock().await;
        cd.record(HARVEST_MULTIPLY_KEY, now);
    }

    let result = outcome.as_audit_string();
    if let EmitOutcome::Failed(_) = outcome {
        warn!(result = %result, "harvest envelope dispatch failed");
    }
    ("releverage_sent", result)
}

async fn dispatch_hedgedjlp_realize(ctx: &HarvestCtx) -> (&'static str, String) {
    let Some(exec) = ctx.execute.as_ref() else {
        return ("realize_dryrun", String::new());
    };

    let now = std::time::SystemTime::now();
    let cooldown_dur = Duration::from_secs(exec.cooldown_secs);
    {
        let cd = exec.cooldown.lock().await;
        if cd.is_cooled_down(HARVEST_HEDGEDJLP_KEY, now, cooldown_dur) {
            let elapsed = cd.seconds_since(HARVEST_HEDGEDJLP_KEY, now).unwrap_or(0);
            info!(
                elapsed_secs = elapsed,
                cooldown_secs = exec.cooldown_secs,
                "harvest hedgedjlp realize skipped — in cooldown",
            );
            return ("realize_cooldown", format!("skipped:cooldown_{elapsed}s"));
        }
    }

    let spec = match build_hedgedjlp_full_withdraw_spec(&exec.targets) {
        Ok(Some(s)) => s,
        Ok(None) => {
            info!("harvest hedgedjlp realize skipped — no hedgedjlp target configured");
            return ("realize_no_target", String::new());
        }
        Err(e) => {
            warn!(?e, "harvest hedgedjlp realize spec build failed");
            return ("realize_error", format!("failed:{e}"));
        }
    };
    info!(label = spec.label, conv = %hex::encode(spec.conv_id), "harvest realize envelope built");

    let outcome = emit_envelope(
        &spec,
        &exec.handle,
        &exec.role_id,
        &exec.nonce,
        exec.wait_for_peer_secs,
    )
    .await;

    {
        let mut cd = exec.cooldown.lock().await;
        cd.record(HARVEST_HEDGEDJLP_KEY, now);
    }

    let result = outcome.as_audit_string();
    if let EmitOutcome::Failed(_) = outcome {
        warn!(result = %result, "harvest hedgedjlp realize envelope dispatch failed");
    }
    ("realize_sent", result)
}

async fn fetch_positions(api_base: &str) -> Result<PositionsView> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()
        .context("build reqwest client")?;
    let url = format!("{}/positions", api_base.trim_end_matches('/'));
    let resp: PositionsView = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?
        .json()
        .await
        .context("decode /positions json")?;
    Ok(resp)
}

async fn fetch_strategies(api_base: &str) -> Result<StrategiesView> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()
        .context("build reqwest client")?;
    let url = format!("{}/strategies", api_base.trim_end_matches('/'));
    let resp: StrategiesView = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?
        .json()
        .await
        .context("decode /strategies json")?;
    Ok(resp)
}

fn append_harvest_audit(path: &std::path::Path, rec: &HarvestRecord<'_>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut line = serde_json::to_string(rec).context("serialize harvest record")?;
    line.push('\n');
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open harvest audit at {}", path.display()))?;
    f.write_all(line.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releverage_skips_no_position() {
        assert!(!should_releverage_multiply(0, 6000, 150));
    }

    #[test]
    fn releverage_skips_zero_target() {
        assert!(!should_releverage_multiply(4500, 0, 150));
    }

    #[test]
    fn releverage_skips_within_band() {
        // 6000 - 5900 = 100 ≤ 150
        assert!(!should_releverage_multiply(5900, 6000, 150));
    }

    #[test]
    fn releverage_skips_at_band_boundary() {
        // strict `>`: drift == threshold → skip (avoid noise on exact match)
        assert!(!should_releverage_multiply(5850, 6000, 150));
    }

    #[test]
    fn releverage_fires_when_drifted_below_band() {
        // 6000 - 4500 = 1500 > 150
        assert!(should_releverage_multiply(4500, 6000, 150));
    }

    #[test]
    fn releverage_fires_just_past_threshold() {
        // 6000 - 5849 = 151 > 150
        assert!(should_releverage_multiply(5849, 6000, 150));
    }

    #[test]
    fn pnl_flag_skips_no_data() {
        assert!(!should_flag_hedgedjlp_pnl(None, 5.0));
    }

    #[test]
    fn pnl_flag_skips_below_threshold() {
        assert!(!should_flag_hedgedjlp_pnl(Some(4.99), 5.0));
    }

    #[test]
    fn pnl_flag_skips_at_threshold() {
        // strict `>` so we don't flap right at the boundary
        assert!(!should_flag_hedgedjlp_pnl(Some(5.0), 5.0));
    }

    #[test]
    fn pnl_flag_fires_above_threshold() {
        assert!(should_flag_hedgedjlp_pnl(Some(10.80), 5.0));
    }

    #[test]
    fn pnl_flag_handles_negative_pnl() {
        // Negative funding paid > threshold? Still skip — we only flag
        // positive earnings to harvest, not losses to realize.
        assert!(!should_flag_hedgedjlp_pnl(Some(-12.0), 5.0));
    }

    #[test]
    fn default_constants_match_user_choice() {
        // Pin the defaults — accidental drift here would change the
        // boot behaviour of every existing systemd unit.
        assert_eq!(DEFAULT_INTERVAL_SECS, 21_600);
        assert_eq!(DEFAULT_LTV_DRIFT_BPS, 150);
        assert!((DEFAULT_PNL_THRESHOLD_USD - 5.0).abs() < f64::EPSILON);
        assert_eq!(DEFAULT_TARGET_LTV_BPS, 6000u16);
    }
}
