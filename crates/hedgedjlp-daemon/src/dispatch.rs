//! Inbox dispatch — decode AssignHedgedJlp / WithdrawHedgedJlp,
//! validate against caps, call jlp_hedge::run_or_simulate or
//! unwind::run_or_simulate, build Report, sign + send.

use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::{info, warn};
use zerox1_defi_runtime::{identity::RoleIdentity, rpc::RpcContext};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::envelope::Envelope;
use zerox1_protocol::fleet::hedgedjlp::{
    AssignHedgedJlp, ReportHedgedJlp, ReportHedgedJlpWithdraw, WithdrawHedgedJlp,
};
use zerox1_protocol::fleet::ReportHeader;
use zerox1_protocol::message::MsgType;

use serde::Serialize;

use crate::caps;

pub struct DispatchCtx {
    pub rpc: Arc<RpcContext>,
    pub wallet: Arc<Wallet>,
    /// Audit-fix I1: SigningWhitelist is wired into the execute path;
    /// every ixn slice passes through `whitelist.verify_ixns` before signing.
    pub whitelist: Arc<SigningWhitelist>,
    pub role_identity: RoleIdentity,
    pub simulate_only: bool,
    pub require_approval: bool,
    pub nonce: Arc<std::sync::atomic::AtomicU64>,
    /// Per-CLI ceiling on USDC the daemon will supply across positions.
    pub args_max_position_usdc_lamports: u64,
    /// Pending-approval queue for AssignHedgedJlp.
    pub assign_queue: Arc<crate::approval::AssignApprovalQueue>,
    /// Parallel queue for WithdrawHedgedJlp payloads. Distinct from
    /// `assign_queue` to keep the audit-fix C1 sender-match check
    /// trivially typed — same generic type, two instances.
    pub withdraw_queue: Arc<crate::approval::WithdrawApprovalQueue>,
    /// Shared rebalance state — needed by the unwind path (M11) to
    /// look up the active position's open hedge legs and to clear
    /// the slot once the unwind submits its close-requests + JLP burn.
    /// Also written by future M11+ assign recorders.
    pub state: Arc<crate::rebalance::RebalanceState>,
}

/// Receive envelopes; dispatch on MsgType::Assign / MsgType::Withdraw /
/// MsgType::Approve with the appropriate CBOR payload.
pub async fn run(mut handle: NodeHandle, ctx: DispatchCtx) -> Result<()> {
    while let Some(env) = handle.recv().await {
        match env.msg_type {
            MsgType::Assign => {
                let conv = env.conversation_id;
                let recipient = env.sender;
                match handle_assign(&handle, &ctx, &env).await {
                    Ok(report) => {
                        let _ = send_report_assign(&handle, &ctx, recipient, conv, report).await;
                    }
                    Err(e) => {
                        warn!(?e, ?conv, "assign failed; sending error Report");
                        let report = ReportHedgedJlp {
                            header: ReportHeader::err(conv, 1),
                            jlp_acquired_lamports: 0,
                            hedge_notional_usdc: 0,
                            current_delta_bps: 0,
                            tx_signatures: vec![],
                        };
                        let _ = send_report_assign(&handle, &ctx, recipient, conv, report).await;
                    }
                }
            }
            MsgType::Withdraw => {
                let conv = env.conversation_id;
                let recipient = env.sender;
                match handle_withdraw(&handle, &ctx, &env).await {
                    Ok(report) => {
                        let _ = send_report_withdraw(&handle, &ctx, recipient, conv, report).await;
                    }
                    Err(e) => {
                        warn!(?e, ?conv, "withdraw failed; sending error Report");
                        let report = ReportHedgedJlpWithdraw {
                            header: ReportHeader::err(conv, 1),
                            usdc_returned_lamports: 0,
                            tx_signatures: vec![],
                        };
                        let _ = send_report_withdraw(&handle, &ctx, recipient, conv, report).await;
                    }
                }
            }
            MsgType::Approve => {
                let conv = env.conversation_id;
                let recipient = env.sender;
                handle_approve(&handle, &ctx, conv, recipient, env.sender).await;
            }
            MsgType::Beacon => { /* role registry observation — M9 */ }
            other => info!(msg_type = ?other, "ignoring inbox envelope"),
        }
    }
    warn!("inbox channel closed; daemon exiting");
    Ok(())
}

/// Drain whichever queue (Assign vs Withdraw) holds a pending entry for
/// `conv` from `sender`. We check the withdraw queue first via the
/// non-destructive contains() helper; on no match, fall through to the
/// assign queue. If neither queue has a match, surface NotFound to logs
/// without replying.
async fn handle_approve(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    conv: [u8; 16],
    recipient: [u8; 32],
    sender: [u8; 32],
) {
    use crate::approval::ApproveResult;

    // Try withdraw queue first if it claims to know this (conv, sender).
    if ctx.withdraw_queue.contains(conv, sender) {
        match ctx.withdraw_queue.approve(conv, sender) {
            ApproveResult::Approved(payload) => {
                info!(?conv, "Approve received — executing queued WithdrawHedgedJlp");
                if let Err(e) = caps::validate_withdraw(&payload) {
                    warn!(?e, ?conv, "post-approval withdraw cap re-validation failed");
                    let report = ReportHedgedJlpWithdraw {
                        header: ReportHeader::err(conv, 3),
                        usdc_returned_lamports: 0,
                        tx_signatures: vec![],
                    };
                    let _ = send_report_withdraw(handle, ctx, recipient, conv, report).await;
                    return;
                }
                match crate::unwind::run_or_simulate(ctx, &ctx.state, &payload, conv).await {
                    Ok(report) => {
                        let _ = send_report_withdraw(handle, ctx, recipient, conv, report).await;
                    }
                    Err(e) => {
                        warn!(?e, ?conv, "queued withdraw failed; sending error Report");
                        let report = ReportHedgedJlpWithdraw {
                            header: ReportHeader::err(conv, 2),
                            usdc_returned_lamports: 0,
                            tx_signatures: vec![],
                        };
                        let _ = send_report_withdraw(handle, ctx, recipient, conv, report).await;
                    }
                }
                return;
            }
            // contains() said yes but approve() saw a TTL race — fall through.
            ApproveResult::NotFound => {}
            ApproveResult::SenderMismatch { expected, got } => {
                warn!(
                    ?conv,
                    expected = %hex::encode(expected),
                    got = %hex::encode(got),
                    "Withdraw Approve REJECTED — sender mismatch."
                );
                return;
            }
        }
    }

    match ctx.assign_queue.approve(conv, sender) {
        ApproveResult::Approved(payload) => {
            info!(?conv, "Approve received — executing queued AssignHedgedJlp");
            // Audit-fix I2: defense in depth — re-validate caps even
            // though we validated on enqueue. Caps are compile-time
            // constants so this is belt-and-suspenders, but cheap.
            if let Err(e) = caps::validate_assign(&payload) {
                warn!(?e, ?conv, "post-approval cap re-validation failed");
                let report = ReportHedgedJlp {
                    header: ReportHeader::err(conv, 3),
                    jlp_acquired_lamports: 0,
                    hedge_notional_usdc: 0,
                    current_delta_bps: 0,
                    tx_signatures: vec![],
                };
                let _ = send_report_assign(handle, ctx, recipient, conv, report).await;
                return;
            }
            match crate::jlp_hedge::run_or_simulate(ctx, &payload, conv).await {
                Ok(report) => {
                    let _ = send_report_assign(handle, ctx, recipient, conv, report).await;
                }
                Err(e) => {
                    warn!(?e, ?conv, "queued assign failed; sending error Report");
                    let report = ReportHedgedJlp {
                        header: ReportHeader::err(conv, 2),
                        jlp_acquired_lamports: 0,
                        hedge_notional_usdc: 0,
                        current_delta_bps: 0,
                        tx_signatures: vec![],
                    };
                    let _ = send_report_assign(handle, ctx, recipient, conv, report).await;
                }
            }
        }
        ApproveResult::NotFound => {
            warn!(?conv, "Approve received but no matching pending Assign or Withdraw (or expired)");
        }
        ApproveResult::SenderMismatch { expected, got } => {
            warn!(
                ?conv,
                expected = %hex::encode(expected),
                got = %hex::encode(got),
                "Approve REJECTED — sender does not match the original sender. \
                 Possible authorization bypass attempt."
            );
            // Don't reply — silence is correct here. The attacker
            // shouldn't get any signal; the legitimate orchestrator
            // can re-Approve.
        }
    }
}

async fn handle_assign(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    env: &Envelope,
) -> Result<ReportHedgedJlp> {
    let payload: AssignHedgedJlp = ciborium::de::from_reader(&env.payload[..])
        .context("decode AssignHedgedJlp CBOR payload")?;

    info!(
        usdc_lamports = payload.usdc_lamports,
        target_delta_bps = payload.target_delta_bps,
        max_borrow_rate_bps = payload.max_borrow_rate_bps,
        deadline_unix = payload.deadline_unix,
        "AssignHedgedJlp received"
    );

    // Cap validation — refuses values above hard caps regardless of orchestrator.
    caps::validate_assign(&payload).context("cap validation")?;

    // Approval gate. When require_approval is true, queue the Assign
    // and emit Escalate(Notice, NeedsApproval) to the orchestrator.
    if ctx.require_approval {
        let conv = env.conversation_id;
        info!(?conv, "AssignHedgedJlp queued — awaiting Approve");
        let added = ctx.assign_queue.enqueue(conv, payload.clone(), env.sender);
        if !added {
            return Err(anyhow!("approval queue full (cap 64); rejecting Assign"));
        }
        // Best-effort emit of the "needs approval" Escalate envelope.
        if let Err(e) = emit_needs_approval(handle, ctx, env).await {
            warn!(?e, ?conv, "failed to emit NeedsApproval Escalate; Assign still queued");
        }
        // Return an "ok=true" Report with zeros to acknowledge the
        // Assign was received and queued.
        return Ok(ReportHedgedJlp {
            header: ReportHeader::ok(conv),
            jlp_acquired_lamports: 0,
            hedge_notional_usdc: 0,
            current_delta_bps: 0,
            tx_signatures: vec![],
        });
    }

    let conv = env.conversation_id;
    crate::jlp_hedge::run_or_simulate(ctx, &payload, conv).await
}

async fn handle_withdraw(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    env: &Envelope,
) -> Result<ReportHedgedJlpWithdraw> {
    let payload: WithdrawHedgedJlp = ciborium::de::from_reader(&env.payload[..])
        .context("decode WithdrawHedgedJlp CBOR payload")?;

    info!(
        jlp_lamports = payload.jlp_lamports,
        deadline_unix = payload.deadline_unix,
        full_withdraw = (payload.jlp_lamports == u64::MAX),
        "WithdrawHedgedJlp received"
    );

    caps::validate_withdraw(&payload).context("withdraw cap validation")?;

    if ctx.require_approval {
        let conv = env.conversation_id;
        info!(?conv, "WithdrawHedgedJlp queued — awaiting Approve");
        let added = ctx.withdraw_queue.enqueue(conv, payload.clone(), env.sender);
        if !added {
            return Err(anyhow!("withdraw approval queue full (cap 64); rejecting Withdraw"));
        }
        if let Err(e) = emit_needs_approval(handle, ctx, env).await {
            warn!(?e, ?conv, "failed to emit NeedsApproval Escalate; Withdraw still queued");
        }
        return Ok(ReportHedgedJlpWithdraw {
            header: ReportHeader::ok(conv),
            usdc_returned_lamports: 0,
            tx_signatures: vec![],
        });
    }

    let conv = env.conversation_id;
    crate::unwind::run_or_simulate(ctx, &ctx.state, &payload, conv).await
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

async fn send_report_assign(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    recipient: [u8; 32],
    conv: [u8; 16],
    report: ReportHedgedJlp,
) -> Result<()> {
    let ok = report.header.ok;
    send_report_inner(handle, ctx, recipient, conv, &report, "ReportHedgedJlp").await?;
    info!(?conv, ok, "assign report sent");
    Ok(())
}

async fn send_report_withdraw(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    recipient: [u8; 32],
    conv: [u8; 16],
    report: ReportHedgedJlpWithdraw,
) -> Result<()> {
    let ok = report.header.ok;
    send_report_inner(handle, ctx, recipient, conv, &report, "ReportHedgedJlpWithdraw").await?;
    info!(?conv, ok, "withdraw report sent");
    Ok(())
}

async fn send_report_inner<R: Serialize>(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    recipient: [u8; 32],
    conv: [u8; 16],
    report: &R,
    label: &'static str,
) -> Result<()> {
    let signing_key =
        ed25519_dalek::SigningKey::from_bytes(ctx.role_identity.signing_key_bytes());
    let sender_pubkey = signing_key.verifying_key().to_bytes();

    let mut payload = Vec::new();
    ciborium::ser::into_writer(report, &mut payload)
        .with_context(|| format!("serialize {label}"))?;

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
    Ok(())
}
