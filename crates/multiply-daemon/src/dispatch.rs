//! Inbox dispatch — decode AssignMultiply, validate against caps,
//! call leverage::run_or_simulate, build ReportMultiply, sign + send.

use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::{info, warn};
use zerox1_defi_runtime::{identity::RoleIdentity, rpc::RpcContext};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::envelope::Envelope;
use zerox1_protocol::fleet::multiply::{AssignMultiply, ReportMultiply};
use zerox1_protocol::fleet::ReportHeader;
use zerox1_protocol::message::MsgType;

use crate::caps;

pub struct DispatchCtx {
    #[allow(dead_code)] // wired in by M6 leverage loop
    pub rpc: Arc<RpcContext>,
    #[allow(dead_code)] // wired in by M6 leverage loop
    pub wallet: Arc<Wallet>,
    #[allow(dead_code)] // wired in by M6 leverage loop
    pub whitelist: Arc<SigningWhitelist>,
    pub role_identity: RoleIdentity,
    pub simulate_only: bool,
    pub require_approval: bool,
    pub nonce: Arc<std::sync::atomic::AtomicU64>,
}

/// Receive envelopes; dispatch on MsgType::Assign with an
/// AssignMultiply CBOR payload.
pub async fn run(mut handle: NodeHandle, ctx: DispatchCtx) -> Result<()> {
    while let Some(env) = handle.recv().await {
        match env.msg_type {
            MsgType::Assign => {
                let conv = env.conversation_id;
                let recipient = env.sender;
                match handle_assign(&ctx, &env).await {
                    Ok(report) => {
                        let _ = send_report(&handle, &ctx, recipient, conv, report).await;
                    }
                    Err(e) => {
                        warn!(?e, ?conv, "assign failed; sending error Report");
                        let report = ReportMultiply {
                            header: ReportHeader::err(conv, 1),
                            resulting_ltv_bps: 0,
                            tx_signature: None,
                        };
                        let _ = send_report(&handle, &ctx, recipient, conv, report).await;
                    }
                }
            }
            MsgType::Beacon => { /* role registry observation — M7 */ }
            other => info!(msg_type = ?other, "ignoring inbox envelope"),
        }
    }
    warn!("inbox channel closed; daemon exiting");
    Ok(())
}

async fn handle_assign(ctx: &DispatchCtx, env: &Envelope) -> Result<ReportMultiply> {
    let payload: AssignMultiply = ciborium::de::from_reader(&env.payload[..])
        .context("decode AssignMultiply CBOR payload")?;

    info!(
        target_ltv_bps = payload.target_ltv_bps,
        max_slippage_bps = payload.max_slippage_bps,
        "AssignMultiply received"
    );

    // Cap validation — refuses values above hard caps regardless of orchestrator.
    caps::validate_assign(&payload).context("cap validation")?;

    // Approval gate. M8 implements the actual queue + Approve handshake.
    // For M4, treat require_approval=true as a refuse-with-error.
    if ctx.require_approval {
        return Err(anyhow!(
            "require_approval is true and Approve flow is not yet wired (M8 lands it)"
        ));
    }

    let conv = env.conversation_id;
    crate::leverage::run_or_simulate(ctx, &payload, conv).await
}

async fn send_report(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    recipient: [u8; 32],
    conv: [u8; 16],
    report: ReportMultiply,
) -> Result<()> {
    let signing_key =
        ed25519_dalek::SigningKey::from_bytes(ctx.role_identity.signing_key_bytes());
    let sender_pubkey = signing_key.verifying_key().to_bytes();

    let mut payload = Vec::new();
    ciborium::ser::into_writer(&report, &mut payload)
        .context("serialize ReportMultiply")?;

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Use an incrementing nonce for bilateral routing validation.
    let nonce = ctx.nonce.fetch_add(1, Ordering::SeqCst);

    let env = Envelope::build(
        MsgType::Report,
        sender_pubkey,
        recipient,
        now_secs,
        nonce,
        conv,
        payload,
        &signing_key,
    );
    handle.send(env).await.context("send Report")?;
    info!(?conv, ok = report.header.ok, "report sent");
    Ok(())
}
