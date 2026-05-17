//! Envelope construction + send for the orchestrator's execute mode.
//!
//! Builds a signed `Envelope` from an `EnvelopeSpec` (produced by
//! `fleet_pm_stub::allocator_runner::action_to_envelope_spec`) and
//! dispatches it through the embedded `NodeHandle`. Mirrors the CLI's
//! `run_allocator` execute path so both code paths land identical
//! bytes on the wire.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

use ed25519_dalek::SigningKey;
use fleet_pm_stub::allocator_runner::EnvelopeSpec;
use zerox1_defi_runtime::identity::RoleIdentity;
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::envelope::Envelope;

/// Outcome string written into the audit record's `envelope_result`.
/// Mirrors the CLI's strings so dashboard ingest can tell them apart.
#[derive(Debug, Clone)]
pub enum EmitOutcome {
    Sent { nonce: u64 },
    Failed(String),
}

impl EmitOutcome {
    pub fn as_audit_string(&self) -> String {
        match self {
            EmitOutcome::Sent { .. } => "sent".to_string(),
            EmitOutcome::Failed(reason) => format!("failed:{reason}"),
        }
    }
}

/// Build, sign, and send the envelope described by `spec`. Bounded wait
/// for the recipient peer is included — `wait_for_peer_secs` is clamped
/// at 60s so a missing recipient never blocks the tick loop for long.
pub async fn emit_envelope(
    spec: &EnvelopeSpec,
    handle: &NodeHandle,
    role_id: &RoleIdentity,
    nonce: &Arc<AtomicU64>,
    wait_for_peer_secs: u64,
) -> EmitOutcome {
    let signing_key = SigningKey::from_bytes(role_id.signing_key_bytes());
    let sender = signing_key.verifying_key().to_bytes();

    let wait = Duration::from_secs(wait_for_peer_secs.min(60));
    if let Err(e) = handle.wait_for_peer(spec.recipient, wait).await {
        warn!(
            ?e,
            label = spec.label,
            "wait_for_peer timed out — sending anyway"
        );
    }

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let nonce_v = nonce.fetch_add(1, Ordering::Relaxed);

    let env = Envelope::build(
        spec.msg_type,
        sender,
        spec.recipient,
        now_secs,
        nonce_v,
        spec.conv_id,
        spec.payload.clone(),
        &signing_key,
    );

    match handle.send(env).await {
        Ok(()) => {
            info!(
                label = spec.label,
                nonce = nonce_v,
                conv = %hex::encode(spec.conv_id),
                "orchestrator envelope sent",
            );
            EmitOutcome::Sent { nonce: nonce_v }
        }
        Err(e) => {
            warn!(?e, label = spec.label, "orchestrator send failed");
            EmitOutcome::Failed(format!("{e}"))
        }
    }
}
