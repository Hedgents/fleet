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
use tracing::{debug, error, info, warn};
use zerox1_defi_protocols::protocols::kamino_loader::DecodedObligation;
use zerox1_defi_runtime::{identity::RoleIdentity, rpc::RpcContext};
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::{
    envelope::Envelope,
    fleet::riskwatcher::{EscalateRisk, RiskKind, RiskSeverity},
    message::MsgType,
};

use crate::caps;

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
}

/// Call once per beacon tick. Reads position; emits Escalate when in
/// warning or critical bands.
pub async fn tick(handle: &NodeHandle, ctx: &LiqMonitorCtx) -> Result<()> {
    // Multiply-daemon's obligation is (tag=0, id=1) — distinct from
    // stable-yield's (0, 0) so a liquidation here cannot seize stable-yield's
    // collateral. See `caps::MULTIPLY_OBLIGATION_SEED` for context.
    let obligation_addr =
        zerox1_defi_protocols::protocols::kamino::derive_user_obligation_with_seed(
            &ctx.user,
            &ctx.lending_market,
            caps::MULTIPLY_OBLIGATION_SEED.0,
            caps::MULTIPLY_OBLIGATION_SEED.1,
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

    if distance_bps <= caps::LIQUIDATION_DISTANCE_CRITICAL_BPS {
        error!(
            distance_bps,
            obligation = %obligation_addr,
            "CRITICAL — position approaching liquidation; auto-unwind not yet implemented (v0.1)"
        );
        emit_escalate(handle, ctx, RiskSeverity::Critical, distance_bps).await?;
    } else if distance_bps <= caps::LIQUIDATION_DISTANCE_WARNING_BPS {
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
        subject: zerox1_defi_protocols::constants::KAMINO_MAIN_MARKET.to_bytes(),
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
