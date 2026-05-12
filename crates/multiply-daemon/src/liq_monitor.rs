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
use zerox1_defi_runtime::{identity::RoleIdentity, rpc::RpcContext};
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::{
    envelope::Envelope,
    fleet::riskwatcher::{EscalateRisk, RiskKind, RiskSeverity},
    message::MsgType,
};

use crate::caps;

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
    let obligation_addr = zerox1_defi_protocols::protocols::kamino::derive_user_obligation(
        &ctx.user,
        &ctx.lending_market,
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
            debug!("liq monitor: no obligation yet, skipping tick");
            return Ok(());
        }
    };

    if decoded.deposited_value_sf == 0 {
        debug!("liq monitor: empty position, skipping tick");
        return Ok(());
    }

    // distance_bps = (1 - borrowed/unhealthy) * 10000.
    // Floor at 0 if borrowed >= unhealthy (already at liquidation line).
    let distance_bps: u16 = if decoded.borrowed_assets_market_value_sf
        >= decoded.unhealthy_borrow_value_sf
    {
        0
    } else if decoded.unhealthy_borrow_value_sf == 0 {
        // No borrow ceiling configured (shouldn't happen on a real reserve);
        // treat as "infinite headroom".
        u16::MAX
    } else {
        let remaining = decoded.unhealthy_borrow_value_sf - decoded.borrowed_assets_market_value_sf;
        let ratio_bps = remaining
            .saturating_mul(10_000)
            .checked_div(decoded.unhealthy_borrow_value_sf)
            .unwrap_or(0);
        ratio_bps.min(u16::MAX as u128) as u16
    };

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
