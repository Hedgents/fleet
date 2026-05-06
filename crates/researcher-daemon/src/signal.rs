//! Build + sign + send MarketSignal envelopes. Researcher's outbound
//! channel — these are how watchers tell the rest of the fleet that a
//! market threshold has crossed.

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

use zerox1_defi_runtime::identity::RoleIdentity;
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::envelope::Envelope;
use zerox1_protocol::fleet::researcher::MarketSignal;
use zerox1_protocol::message::MsgType;

use crate::telemetry::{self, TelemetryHandle};

/// Build, sign, and send a MarketSignal envelope to a single recipient.
/// Caller is responsible for de-dup (see `dedup::EmissionTracker`).
///
/// `telemetry` is optional — when `Some`, a JSONL line is appended and
/// the tally bumped after a successful send. Telemetry I/O failures
/// are logged but never propagated (signal emission is the primary
/// success path).
///
/// `conv_id` may be a per-(kind, asset) identifier so subscribers can
/// correlate; in v0 we just use the all-zeros conv since signals are
/// fire-and-forget broadcasts (each is independently meaningful).
pub async fn emit_to(
    handle: &NodeHandle,
    role: &RoleIdentity,
    nonce: &Arc<AtomicU64>,
    recipient: [u8; 32],
    payload: MarketSignal,
    telemetry: Option<&Arc<TelemetryHandle>>,
) -> Result<()> {
    let signing_key = ed25519_dalek::SigningKey::from_bytes(role.signing_key_bytes());
    let sender = signing_key.verifying_key().to_bytes();

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut payload_bytes = Vec::new();
    ciborium::ser::into_writer(&payload, &mut payload_bytes)
        .context("serialize MarketSignal")?;

    let nonce_v = nonce.fetch_add(1, Ordering::Relaxed);

    let env = Envelope::build(
        MsgType::MarketSignal,
        sender,
        recipient,
        now_secs,
        nonce_v,
        [0u8; 16], // conv_id unused for signals
        payload_bytes,
        &signing_key,
    );
    handle.send(env).await.context("send MarketSignal")?;

    info!(
        kind = ?payload.kind,
        asset = ?payload.asset,
        severity = ?payload.severity,
        measurement_bps = payload.measurement_bps,
        recipient = %hex::encode(&recipient[..8]),
        "MarketSignal emitted"
    );

    if let Some(t) = telemetry {
        if let Err(e) = telemetry::record_emission(
            &t.log_path,
            &t.log_writer,
            &t.tally,
            &payload,
            1,
        )
        .await
        {
            warn!(?e, "telemetry record_emission failed (non-fatal)");
        }
    }
    Ok(())
}

/// Emit a MarketSignal to MULTIPLE recipients. Iterates emit_to; logs
/// errors but does not abort on individual failures (one consumer being
/// offline shouldn't block the rest).
///
/// Telemetry is recorded once per call (with `recipient_count =
/// recipients.len()`) rather than per-recipient — fan-out shouldn't
/// inflate the per-signal tally. To avoid double-counting, the
/// inner per-recipient `emit_to` calls are passed `None`.
pub async fn emit_broadcast(
    handle: &NodeHandle,
    role: &RoleIdentity,
    nonce: &Arc<AtomicU64>,
    recipients: &[[u8; 32]],
    payload: MarketSignal,
    telemetry: Option<&Arc<TelemetryHandle>>,
) -> usize {
    let mut sent = 0;
    for r in recipients {
        // Pass None to inner call — telemetry recorded once below.
        match emit_to(handle, role, nonce, *r, payload.clone(), None).await {
            Ok(()) => sent += 1,
            Err(e) => tracing::warn!(?e, recipient = %hex::encode(&r[..8]), "MarketSignal send failed"),
        }
    }

    if sent > 0 {
        if let Some(t) = telemetry {
            if let Err(e) = telemetry::record_emission(
                &t.log_path,
                &t.log_writer,
                &t.tally,
                &payload,
                recipients.len(),
            )
            .await
            {
                warn!(?e, "telemetry record_emission failed (non-fatal)");
            }
        }
    }

    sent
}
