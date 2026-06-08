//! Liquidation-distance monitor.
//!
//! On every beacon tick, query the user's Kamino obligation and compute
//! distance-to-liquidation in basis points. Emit Escalate envelopes when
//! the position drifts into warning or critical bands.
//!
//! Auto-unwind on Critical is deferred to v0.1 — v0 emits Escalate(Critical)
//! and relies on operator intervention.

use anyhow::{Context, Result};
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use zerox1_defi_protocols::constants::KAMINO_ONYC_RESERVE;
use zerox1_defi_protocols::protocols::kamino_loader::DecodedObligation;
use zerox1_defi_runtime::{identity::RoleIdentity, rpc::RpcContext};
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::{
    envelope::Envelope,
    fleet::riskwatcher::{EscalateRisk, RiskKind, RiskSeverity},
    message::MsgType,
};

use crate::caps;
use crate::nav_controller::{
    self, decide_nav_response, nav_micro_usd_from_deposit, NavHistory, NavResponse,
    DEFAULT_MAX_CONSECUTIVE_DOWN_STEPS, DEFAULT_SINGLE_STEP_ALARM_BPS, DEFAULT_STEP_BUFFER_BPS,
};

/// Returns `true` iff `decoded` represents an active leveraged position —
/// non-zero collateral AND a non-zero unhealthy borrow ceiling.
///
/// v0.1.11 Bug 2 fix: previously, when the obligation existed but had no
/// collateral or no borrow ceiling, `distance_bps` floored to 0 and the
/// monitor emitted Critical(0) every tick. This predicate is now the gate
/// that suppresses emission for empty / inactive obligations.
pub(crate) fn obligation_has_active_position(decoded: &DecodedObligation) -> bool {
    let has_collateral =
        decoded.deposited_value_sf > 0 && decoded.deposits.iter().any(|d| d.deposited_amount > 0);
    let has_borrow_ceiling = decoded.unhealthy_borrow_value_sf > 0;
    has_collateral && has_borrow_ceiling
}

/// Pure compute of distance-to-liquidation in basis points, given an
/// obligation already known to represent an active position.
pub(crate) fn compute_distance_bps(decoded: &DecodedObligation) -> u16 {
    if decoded.borrowed_assets_market_value_sf >= decoded.unhealthy_borrow_value_sf {
        return 0;
    }
    let remaining = decoded.unhealthy_borrow_value_sf - decoded.borrowed_assets_market_value_sf;
    let ratio_bps = remaining
        .saturating_mul(10_000)
        .checked_div(decoded.unhealthy_borrow_value_sf)
        .unwrap_or(0);
    ratio_bps.min(u16::MAX as u128) as u16
}

pub struct LiqMonitorCtx {
    pub rpc: Arc<RpcContext>,
    pub user: Pubkey,
    pub lending_market: Pubkey,
    pub role_identity: RoleIdentity,
    pub orchestrator_agent_id: Option<[u8; 32]>,
    pub outbound_nonce: Arc<std::sync::atomic::AtomicU64>,
    /// v0.4.29c: shared NAV history for the NAV-aware controller. Each
    /// tick pushes the latest computed NAV onto this rolling window and
    /// asks the controller whether to dampen or escalate alerts.
    pub nav_history: Arc<Mutex<NavHistory>>,
}

/// Call once per beacon tick. Reads position; emits Escalate when in
/// warning or critical bands.
pub async fn tick(handle: &NodeHandle, ctx: &LiqMonitorCtx) -> Result<()> {
    // Multiply-daemon's obligation is (tag=0, id=1) — distinct from
    // stable-yield's (0, 0) so a liquidation here cannot seize stable-yield's
    // collateral. See `caps::ONYC_OBLIGATION_SEED` for context.
    let obligation_addr =
        zerox1_defi_protocols::protocols::kamino::derive_user_obligation_with_seed(
            &ctx.user,
            &ctx.lending_market,
            caps::ONYC_OBLIGATION_SEED.0,
            caps::ONYC_OBLIGATION_SEED.1,
        );

    let decoded = match zerox1_defi_protocols::protocols::kamino_loader::fetch_obligation(
        &ctx.rpc.client,
        &obligation_addr,
    )
    .await
    .context("fetch obligation")?
    {
        Some(d) => d,
        None => {
            debug!("liq monitor: no active position, skipping tick");
            return Ok(());
        }
    };

    // v0.1.11 Bug 2 fix: do NOT emit Escalate when there's no active position.
    // Prior bug: when the obligation existed but had zero collateral (or zero
    // borrow ceiling), distance_bps floored to 0 and we emitted Critical(0)
    // every tick. See `obligation_has_active_position` for the gate.
    if !obligation_has_active_position(&decoded) {
        debug!(
            deposited_value_sf = %decoded.deposited_value_sf,
            unhealthy_borrow_value_sf = %decoded.unhealthy_borrow_value_sf,
            "liq monitor: no active position, skipping tick"
        );
        return Ok(());
    }

    let distance_bps = compute_distance_bps(&decoded);

    // v0.4.29c: NAV-aware gating. Compute the ONyc per-token NAV from
    // the obligation's deposit slot, push to history, ask the
    // controller. Dampen alerts when the latest move is a single
    // discrete downward step within the buffer — the controller waits
    // to see if it's transient or the start of sustained drift before
    // letting auto-unwind fire.
    let nav_response = match update_nav_history_and_check(&decoded, &ctx.nav_history).await {
        Ok(r) => r,
        Err(e) => {
            warn!(
                ?e,
                "nav_controller update failed; falling through to standard alert path"
            );
            NavResponse::Normal
        }
    };

    let should_emit_critical = distance_bps <= caps::LIQUIDATION_DISTANCE_CRITICAL_BPS;
    let should_emit_warning = distance_bps <= caps::LIQUIDATION_DISTANCE_WARNING_BPS;

    if let NavResponse::DampenAlerts {
        step_delta_bps,
        streak,
    } = nav_response
    {
        // Single discrete NAV step beyond the noise buffer but neither
        // a single-event alarm nor a sustained-drift streak. Log loudly
        // so the operator sees the dampening — but DO NOT emit Escalate.
        if should_emit_critical || should_emit_warning {
            info!(
                step_delta_bps,
                streak,
                distance_bps,
                obligation = %obligation_addr,
                "NAV-aware controller DAMPENED an alert this tick — single discrete \
                 downward step within buffer; awaiting confirmation of sustained drift"
            );
        }
        return Ok(());
    }

    if let NavResponse::EscalateAlerts {
        step_delta_bps,
        streak,
        reason,
    } = nav_response
    {
        info!(
            step_delta_bps,
            streak,
            ?reason,
            distance_bps,
            obligation = %obligation_addr,
            "NAV-aware controller ESCALATED — real impairment detected, alerts pass through"
        );
    }

    if should_emit_critical {
        error!(
            distance_bps,
            obligation = %obligation_addr,
            "CRITICAL — position approaching liquidation; auto-unwind not yet implemented (v0.1)"
        );
        emit_escalate(handle, ctx, RiskSeverity::Critical, distance_bps).await?;
    } else if should_emit_warning {
        warn!(
            distance_bps,
            obligation = %obligation_addr,
            "WARNING — position drift; emit Escalate"
        );
        emit_escalate(handle, ctx, RiskSeverity::Warning, distance_bps).await?;
    } else {
        info!(
            distance_bps,
            obligation = %obligation_addr,
            "liq monitor: position healthy"
        );
    }

    Ok(())
}

/// Compute the current ONyc per-token NAV from the obligation's deposit
/// slot, push to history, and return the controller's response.
async fn update_nav_history_and_check(
    decoded: &DecodedObligation,
    nav_history: &Arc<Mutex<NavHistory>>,
) -> Result<NavResponse> {
    let onyc_slot = decoded
        .deposits
        .iter()
        .find(|d| d.reserve == KAMINO_ONYC_RESERVE && d.deposited_amount > 0);
    let Some(slot) = onyc_slot else {
        // No ONyc deposit — nothing to track. Treat as Normal.
        return Ok(NavResponse::Normal);
    };
    // ONyc has 9 decimals.
    let nav_micro = match nav_micro_usd_from_deposit(slot.deposited_amount, slot.market_value_sf, 9)
    {
        Some(n) => n,
        None => return Ok(NavResponse::Normal),
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut hist = nav_history.lock().await;
    hist.observe(now, nav_micro);
    Ok(decide_nav_response(
        &hist,
        DEFAULT_STEP_BUFFER_BPS,
        DEFAULT_SINGLE_STEP_ALARM_BPS,
        DEFAULT_MAX_CONSECUTIVE_DOWN_STEPS,
    ))
}

async fn emit_escalate(
    handle: &NodeHandle,
    ctx: &LiqMonitorCtx,
    severity: RiskSeverity,
    distance_bps: u16,
) -> Result<()> {
    let Some(recipient) = ctx.orchestrator_agent_id else {
        debug!(
            ?severity,
            distance_bps,
            "liq monitor: no --orchestrator-agent-id configured; skipping mesh Escalate (log-only)"
        );
        return Ok(());
    };

    let signing_key = ed25519_dalek::SigningKey::from_bytes(ctx.role_identity.signing_key_bytes());
    let sender = signing_key.verifying_key().to_bytes();

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let payload = EscalateRisk {
        severity,
        kind: RiskKind::LiquidationDistance,
        subject: zerox1_defi_protocols::constants::KAMINO_ONYC_MARKET.to_bytes(),
        measurement: distance_bps as i64,
        raised_at_unix: now_secs,
    };

    let mut payload_bytes = Vec::new();
    ciborium::ser::into_writer(&payload, &mut payload_bytes)?;

    let nonce = ctx
        .outbound_nonce
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let env = Envelope::build(
        MsgType::Escalate,
        sender,
        recipient,
        now_secs,
        nonce,
        [0u8; 16], // no conversation_id for proactive escalates
        payload_bytes,
        &signing_key,
    );
    handle.send(env).await.context("send Escalate")?;
    info!(?severity, distance_bps, "Escalate envelope sent");
    Ok(())
}

#[cfg(test)]
mod tests {
    //! v0.1.11 Bug 2: prove that an obligation with no active position is
    //! gated out of Escalate emission, while a near-liquidation position
    //! produces a Critical-band distance_bps.
    use super::*;
    use zerox1_defi_protocols::protocols::kamino_loader::{
        DecodedObligation, ObligationBorrow, ObligationDeposit,
    };

    fn mk_obligation(
        deposits: Vec<ObligationDeposit>,
        borrows: Vec<ObligationBorrow>,
        deposited_value_sf: u128,
        borrowed_assets_market_value_sf: u128,
        unhealthy_borrow_value_sf: u128,
    ) -> DecodedObligation {
        DecodedObligation {
            address: Pubkey::new_unique(),
            lending_market: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            deposits,
            borrows,
            deposited_value_sf,
            borrow_factor_adjusted_debt_value_sf: borrowed_assets_market_value_sf,
            borrowed_assets_market_value_sf,
            allowed_borrow_value_sf: unhealthy_borrow_value_sf,
            unhealthy_borrow_value_sf,
        }
    }

    fn deposit(amount: u64) -> ObligationDeposit {
        ObligationDeposit {
            reserve: Pubkey::new_unique(),
            deposited_amount: amount,
            market_value_sf: amount as u128,
        }
    }

    #[test]
    fn empty_obligation_has_no_active_position() {
        // Just-initialised obligation: no deposits, no borrow ceiling.
        // This is the bug case — pre-fix, the tick emitted Critical(0).
        let ob = mk_obligation(vec![], vec![], 0, 0, 0);
        assert!(!obligation_has_active_position(&ob));
    }

    #[test]
    fn deposit_only_no_borrow_ceiling_is_inactive() {
        // Obligation has collateral but reserve config gives no borrow
        // ceiling — treat as inactive rather than infinitely-healthy
        // (the prior u16::MAX branch). Still no Escalate-worthy state.
        let ob = mk_obligation(vec![deposit(1)], vec![], 1, 0, 0);
        assert!(!obligation_has_active_position(&ob));
    }

    #[test]
    fn deposit_with_zero_amount_is_inactive() {
        // Stale empty deposit slot — every deposited_amount is zero —
        // is NOT an active position.
        let ob = mk_obligation(vec![deposit(0)], vec![], 0, 0, 100);
        assert!(!obligation_has_active_position(&ob));
    }

    #[test]
    fn healthy_position_is_active_and_distance_is_high() {
        // 100 collateral, 12 borrowed, 90 unhealthy ceiling →
        // remaining = 78, distance ≈ 78/90 = 8666 bps. Far above warning.
        let ob = mk_obligation(vec![deposit(1)], vec![], 100, 12, 90);
        assert!(obligation_has_active_position(&ob));
        let d = compute_distance_bps(&ob);
        assert!(d > caps::LIQUIDATION_DISTANCE_WARNING_BPS);
    }

    #[test]
    fn near_liquidation_position_yields_critical_distance() {
        // borrowed ≈ unhealthy ceiling. distance_bps should land at or below
        // the Critical threshold — proving the real-Critical path still works.
        let ob = mk_obligation(vec![deposit(1)], vec![], 1000, 999, 1000);
        assert!(obligation_has_active_position(&ob));
        let d = compute_distance_bps(&ob);
        assert!(
            d <= caps::LIQUIDATION_DISTANCE_CRITICAL_BPS,
            "expected critical distance, got {d}"
        );
    }

    #[test]
    fn borrowed_above_ceiling_distance_is_zero() {
        // Already past the liquidation line: borrowed > unhealthy ceiling.
        let ob = mk_obligation(vec![deposit(1)], vec![], 100, 95, 90);
        assert!(obligation_has_active_position(&ob));
        assert_eq!(compute_distance_bps(&ob), 0);
    }
}
