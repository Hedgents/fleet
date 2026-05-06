//! Inbox dispatch — decode AssignStableLend, validate against caps,
//! call lend::run_or_simulate, build ReportStableLend, sign + send.

use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::{info, warn};
use zerox1_defi_runtime::{identity::RoleIdentity, rpc::RpcContext};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::envelope::Envelope;
use zerox1_protocol::fleet::stable_lend::{AssignStableLend, ReportStableLend};
use zerox1_protocol::fleet::ReportHeader;
use zerox1_protocol::message::MsgType;

use crate::caps;

pub struct DispatchCtx {
    pub rpc: Arc<RpcContext>,
    pub wallet: Arc<Wallet>,
    /// Audit-fix I1: SigningWhitelist is wired into the lend loop;
    /// every ixn slice passes through `whitelist.verify_ixns` before signing.
    pub whitelist: Arc<SigningWhitelist>,
    pub role_identity: RoleIdentity,
    pub simulate_only: bool,
    pub require_approval: bool,
    pub nonce: Arc<std::sync::atomic::AtomicU64>,
    /// Per-CLI ceiling on USDC the daemon will supply across positions.
    /// The lend loop uses this to clamp each Assign's deposit amount.
    pub args_max_position_usdc_lamports: u64,
    /// Pending-approval queue. When `require_approval=true`, incoming
    /// Assigns land here and wait for a matching Approve envelope.
    pub approval_queue: Arc<crate::approval::ApprovalQueue>,
}

/// Receive envelopes; dispatch on MsgType::Assign with an
/// AssignStableLend CBOR payload.
pub async fn run(mut handle: NodeHandle, ctx: DispatchCtx) -> Result<()> {
    while let Some(env) = handle.recv().await {
        match env.msg_type {
            MsgType::Assign => {
                let conv = env.conversation_id;
                let recipient = env.sender;
                match handle_assign(&handle, &ctx, &env).await {
                    Ok(report) => {
                        let _ = send_report(&handle, &ctx, recipient, conv, report).await;
                    }
                    Err(e) => {
                        warn!(?e, ?conv, "assign failed; sending error Report");
                        let report = ReportStableLend {
                            header: ReportHeader::err(conv, 1),
                            deposited_usdc_lamports: 0,
                            current_apr_bps: 0,
                            tx_signature: None,
                        };
                        let _ = send_report(&handle, &ctx, recipient, conv, report).await;
                    }
                }
            }
            MsgType::Approve => {
                let conv = env.conversation_id;
                let recipient = env.sender;
                match ctx.approval_queue.approve(conv, env.sender) {
                    crate::approval::ApproveResult::Approved(payload) => {
                        info!(?conv, "Approve received — executing queued AssignStableLend");
                        // Audit-fix I2: defense in depth — re-validate caps even
                        // though we validated on enqueue. Caps are compile-time
                        // constants so this is belt-and-suspenders, but cheap.
                        if let Err(e) = caps::validate_assign(&payload) {
                            warn!(?e, ?conv, "post-approval cap re-validation failed");
                            let report = ReportStableLend {
                                header: ReportHeader::err(conv, 3),
                                deposited_usdc_lamports: 0,
                                current_apr_bps: 0,
                                tx_signature: None,
                            };
                            let _ = send_report(&handle, &ctx, recipient, conv, report).await;
                            continue;
                        }
                        match crate::lend::run_or_simulate(&ctx, &payload, conv).await {
                            Ok(report) => {
                                let _ = send_report(&handle, &ctx, recipient, conv, report).await;
                            }
                            Err(e) => {
                                warn!(?e, ?conv, "queued assign failed; sending error Report");
                                let report = ReportStableLend {
                                    header: ReportHeader::err(conv, 2),
                                    deposited_usdc_lamports: 0,
                                    current_apr_bps: 0,
                                    tx_signature: None,
                                };
                                let _ = send_report(&handle, &ctx, recipient, conv, report).await;
                            }
                        }
                    }
                    crate::approval::ApproveResult::NotFound => {
                        warn!(?conv, "Approve received but no matching pending Assign (or expired)");
                    }
                    crate::approval::ApproveResult::SenderMismatch { expected, got } => {
                        warn!(
                            ?conv,
                            expected = %hex::encode(expected),
                            got = %hex::encode(got),
                            "Approve REJECTED — sender does not match the original Assign sender. \
                             Possible authorization bypass attempt."
                        );
                        // Don't reply — silence is correct here. The attacker
                        // shouldn't get any signal; the legitimate orchestrator
                        // can re-Approve.
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

async fn handle_assign(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    env: &Envelope,
) -> Result<ReportStableLend> {
    let payload: AssignStableLend = ciborium::de::from_reader(&env.payload[..])
        .context("decode AssignStableLend CBOR payload")?;

    info!(
        usdc_lamports = payload.usdc_lamports,
        deadline_unix = payload.deadline_unix,
        "AssignStableLend received"
    );

    // Cap validation — refuses values above hard caps regardless of orchestrator.
    caps::validate_assign(&payload).context("cap validation")?;

    // Approval gate. When require_approval is true, queue the Assign
    // and emit Escalate(Notice, NeedsApproval) to the orchestrator.
    if ctx.require_approval {
        let conv = env.conversation_id;
        info!(?conv, "AssignStableLend queued — awaiting Approve");
        let added = ctx.approval_queue.enqueue(conv, payload.clone(), env.sender);
        if !added {
            return Err(anyhow!("approval queue full (cap 64); rejecting Assign"));
        }
        // Best-effort emit of the "needs approval" Escalate envelope.
        if let Err(e) = emit_needs_approval(handle, ctx, env).await {
            warn!(?e, ?conv, "failed to emit NeedsApproval Escalate; Assign still queued");
        }
        // Return an "ok=true" Report with deposited=0 + tx_signature=None
        // to acknowledge the Assign was received and queued.
        return Ok(ReportStableLend {
            header: ReportHeader::ok(conv),
            deposited_usdc_lamports: 0,
            current_apr_bps: 0,
            tx_signature: None,
        });
    }

    let conv = env.conversation_id;
    crate::lend::run_or_simulate(ctx, &payload, conv).await
}

/// Build + send an Escalate envelope of kind `NeedsApproval`, routed back
/// to the orchestrator that issued the Assign. Re-uses the Assign's
/// conversation_id so the orchestrator can correlate.
async fn emit_needs_approval(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    env: &Envelope,
) -> Result<()> {
    use zerox1_protocol::fleet::riskwatcher::{EscalateRisk, RiskKind, RiskSeverity};

    let signing_key =
        ed25519_dalek::SigningKey::from_bytes(ctx.role_identity.signing_key_bytes());
    let sender = signing_key.verifying_key().to_bytes();
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let payload = EscalateRisk {
        severity: RiskSeverity::Notice,
        kind: RiskKind::NeedsApproval,
        // No specific subject — the conversation_id is the correlation key.
        subject: [0u8; 32],
        measurement: 0,
        raised_at_unix: now_secs,
    };
    let mut payload_bytes = Vec::new();
    ciborium::ser::into_writer(&payload, &mut payload_bytes)
        .context("serialize NeedsApproval EscalateRisk")?;

    let nonce = ctx.nonce.fetch_add(1, Ordering::Relaxed);

    let env_out = Envelope::build(
        MsgType::Escalate,
        sender,
        env.sender, // route back to the orchestrator that sent the Assign
        now_secs,
        nonce,
        env.conversation_id, // re-use Assign's conv_id for correlation
        payload_bytes,
        &signing_key,
    );
    handle.send(env_out).await.context("send NeedsApproval Escalate")?;
    info!(conv = ?env.conversation_id, "NeedsApproval Escalate emitted");
    Ok(())
}

async fn send_report(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    recipient: [u8; 32],
    conv: [u8; 16],
    report: ReportStableLend,
) -> Result<()> {
    let signing_key =
        ed25519_dalek::SigningKey::from_bytes(ctx.role_identity.signing_key_bytes());
    let sender_pubkey = signing_key.verifying_key().to_bytes();

    let mut payload = Vec::new();
    ciborium::ser::into_writer(&report, &mut payload)
        .context("serialize ReportStableLend")?;

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
