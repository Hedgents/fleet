//! Inbox dispatch — decode AssignStableLend, validate against caps,
//! call lend::run_or_simulate, build ReportStableLend, sign + send.

use anyhow::{anyhow, Context, Result};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::{info, warn};
use zerox1_defi_runtime::{identity::RoleIdentity, rpc::RpcContext};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::envelope::Envelope;
use zerox1_protocol::fleet::stable_lend::{
    AssignStableLend, ReportStableLend, ReportStableWithdraw, WithdrawStableLend,
};
use zerox1_protocol::fleet::ReportHeader;
use zerox1_protocol::message::MsgType;

use serde::Serialize;

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
    pub approval_queue: Arc<crate::approval::AssignApprovalQueue>,
    /// Parallel queue for WithdrawStableLend payloads (M10). Distinct from
    /// `approval_queue` to keep the audit-fix C1 sender-match check
    /// trivially typed — same generic type, two instances.
    pub withdraw_queue: Arc<crate::approval::WithdrawApprovalQueue>,
    /// Audit-fix C1: 32-byte pubkey of the orchestrator authorised to send
    /// Assign / Withdraw envelopes. Required on mainnet (enforced in main.rs).
    /// When `None` (devnet sandbox), the sender allowlist is disabled.
    /// Unauthorised envelopes are warned-and-dropped — matches the Approve
    /// sender-mismatch shape (no error Report sent back to the attacker).
    pub orchestrator_agent_id: Option<[u8; 32]>,
}

/// Audit-fix C1: returns `true` iff `sender` is authorised under the
/// orchestrator allowlist. `expected = None` (no orchestrator configured —
/// devnet sandbox) means every sender passes. Unauthorised envelopes are
/// loudly warned; the caller silently drops them — same shape as the
/// Approve sender-mismatch branch, so a probing attacker gets no signal back.
fn sender_is_authorised(expected: Option<[u8; 32]>, sender: [u8; 32], kind: &'static str) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    if sender == expected {
        return true;
    }
    warn!(
        msg = kind,
        sender = %hex::encode(sender),
        expected = %hex::encode(expected),
        "{} REJECTED — sender does not match configured orchestrator. \
         Possible authorization bypass attempt.",
        kind,
    );
    false
}

/// Returns `true` if the envelope's payload cleanly CBOR-decodes as a
/// stable-yield Assign/Withdraw type. Defence-in-depth (Fix 3a,
/// 2026-05-13): silently drop unrelated payloads so the daemon doesn't
/// send error Reports for Assigns aimed at other desks.
fn payload_is_for_this_daemon(env: &Envelope) -> bool {
    match env.msg_type {
        MsgType::Assign => {
            ciborium::de::from_reader::<AssignStableLend, _>(&env.payload[..]).is_ok()
        }
        MsgType::Withdraw => {
            ciborium::de::from_reader::<WithdrawStableLend, _>(&env.payload[..]).is_ok()
        }
        _ => true,
    }
}

/// Receive envelopes; dispatch on MsgType::Assign / MsgType::Withdraw
/// with the appropriate CBOR payload.
pub async fn run(mut handle: NodeHandle, ctx: DispatchCtx) -> Result<()> {
    while let Some(env) = handle.recv().await {
        if !payload_is_for_this_daemon(&env) {
            tracing::debug!(
                msg_type = ?env.msg_type,
                sender = %hex::encode(env.sender),
                "envelope payload not for this daemon; dropping silently"
            );
            continue;
        }
        match env.msg_type {
            MsgType::Assign => {
                let conv = env.conversation_id;
                let recipient = env.sender;
                if !sender_is_authorised(ctx.orchestrator_agent_id, env.sender, "Assign") {
                    continue;
                }
                match handle_assign(&handle, &ctx, &env).await {
                    Ok(report) => {
                        let _ = send_report_assign(&handle, &ctx, recipient, conv, report).await;
                    }
                    Err(e) => {
                        warn!(?e, ?conv, "assign failed; sending error Report");
                        let report = ReportStableLend {
                            header: ReportHeader::err(conv, 1),
                            deposited_usdc_lamports: 0,
                            current_apr_bps: 0,
                            tx_signature: None,
                        };
                        let _ = send_report_assign(&handle, &ctx, recipient, conv, report).await;
                    }
                }
            }
            MsgType::Withdraw => {
                let conv = env.conversation_id;
                let recipient = env.sender;
                if !sender_is_authorised(ctx.orchestrator_agent_id, env.sender, "Withdraw") {
                    continue;
                }
                match handle_withdraw(&handle, &ctx, &env).await {
                    Ok(report) => {
                        let _ = send_report_withdraw(&handle, &ctx, recipient, conv, report).await;
                    }
                    Err(e) => {
                        warn!(?e, ?conv, "withdraw failed; sending error Report");
                        let report = ReportStableWithdraw {
                            header: ReportHeader::err(conv, 1),
                            withdrawn_usdc_lamports: 0,
                            tx_signature: None,
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
            MsgType::Beacon => { /* role registry observation — M7 */ }
            other => info!(msg_type = ?other, "ignoring inbox envelope"),
        }
    }
    warn!("inbox channel closed; daemon exiting");
    Ok(())
}

/// Drain whichever queue (Assign vs Withdraw) holds a pending entry for
/// `conv` from `sender`. We try the withdraw queue first then the assign
/// queue — order is arbitrary (a given `conv` only ever exists in ONE
/// queue at a time because conv_ids are nonces). If neither queue has a
/// match, surface NotFound to logs without replying.
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
                info!(
                    ?conv,
                    "Approve received — executing queued WithdrawStableLend"
                );
                if let Err(e) = caps::validate_withdraw(&payload) {
                    warn!(?e, ?conv, "post-approval withdraw cap re-validation failed");
                    let report = ReportStableWithdraw {
                        header: ReportHeader::err(conv, 3),
                        withdrawn_usdc_lamports: 0,
                        tx_signature: None,
                    };
                    let _ = send_report_withdraw(handle, ctx, recipient, conv, report).await;
                    return;
                }
                match crate::lend::run_withdraw_or_simulate(ctx, &payload, conv).await {
                    Ok(report) => {
                        let _ = send_report_withdraw(handle, ctx, recipient, conv, report).await;
                    }
                    Err(e) => {
                        warn!(?e, ?conv, "queued withdraw failed; sending error Report");
                        let report = ReportStableWithdraw {
                            header: ReportHeader::err(conv, 2),
                            withdrawn_usdc_lamports: 0,
                            tx_signature: None,
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

    match ctx.approval_queue.approve(conv, sender) {
        ApproveResult::Approved(payload) => {
            info!(
                ?conv,
                "Approve received — executing queued AssignStableLend"
            );
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
                let _ = send_report_assign(handle, ctx, recipient, conv, report).await;
                return;
            }
            match crate::lend::run_or_simulate(ctx, &payload, conv).await {
                Ok(report) => {
                    let _ = send_report_assign(handle, ctx, recipient, conv, report).await;
                }
                Err(e) => {
                    warn!(?e, ?conv, "queued assign failed; sending error Report");
                    let report = ReportStableLend {
                        header: ReportHeader::err(conv, 2),
                        deposited_usdc_lamports: 0,
                        current_apr_bps: 0,
                        tx_signature: None,
                    };
                    let _ = send_report_assign(handle, ctx, recipient, conv, report).await;
                }
            }
        }
        ApproveResult::NotFound => {
            warn!(
                ?conv,
                "Approve received but no matching pending Assign/Withdraw (or expired)"
            );
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
        let added = ctx
            .approval_queue
            .enqueue(conv, payload.clone(), env.sender);
        if !added {
            return Err(anyhow!("approval queue full (cap 64); rejecting Assign"));
        }
        // Best-effort emit of the "needs approval" Escalate envelope.
        if let Err(e) = emit_needs_approval(handle, ctx, env).await {
            warn!(
                ?e,
                ?conv,
                "failed to emit NeedsApproval Escalate; Assign still queued"
            );
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
async fn emit_needs_approval(handle: &NodeHandle, ctx: &DispatchCtx, env: &Envelope) -> Result<()> {
    use zerox1_protocol::fleet::riskwatcher::{EscalateRisk, RiskKind, RiskSeverity};

    let signing_key = ed25519_dalek::SigningKey::from_bytes(ctx.role_identity.signing_key_bytes());
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
    handle
        .send(env_out)
        .await
        .context("send NeedsApproval Escalate")?;
    info!(conv = ?env.conversation_id, "NeedsApproval Escalate emitted");
    Ok(())
}

async fn handle_withdraw(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    env: &Envelope,
) -> Result<ReportStableWithdraw> {
    let payload: WithdrawStableLend = ciborium::de::from_reader(&env.payload[..])
        .context("decode WithdrawStableLend CBOR payload")?;

    info!(
        usdc_lamports = payload.usdc_lamports,
        deadline_unix = payload.deadline_unix,
        full_withdraw = (payload.usdc_lamports == u64::MAX),
        "WithdrawStableLend received"
    );

    caps::validate_withdraw(&payload).context("withdraw cap validation")?;

    if ctx.require_approval {
        let conv = env.conversation_id;
        info!(?conv, "WithdrawStableLend queued — awaiting Approve");
        let added = ctx
            .withdraw_queue
            .enqueue(conv, payload.clone(), env.sender);
        if !added {
            return Err(anyhow!(
                "withdraw approval queue full (cap 64); rejecting Withdraw"
            ));
        }
        if let Err(e) = emit_needs_approval(handle, ctx, env).await {
            warn!(
                ?e,
                ?conv,
                "failed to emit NeedsApproval Escalate; Withdraw still queued"
            );
        }
        return Ok(ReportStableWithdraw {
            header: ReportHeader::ok(conv),
            withdrawn_usdc_lamports: 0,
            tx_signature: None,
        });
    }

    let conv = env.conversation_id;
    crate::lend::run_withdraw_or_simulate(ctx, &payload, conv).await
}

async fn send_report_assign(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    recipient: [u8; 32],
    conv: [u8; 16],
    report: ReportStableLend,
) -> Result<()> {
    let ok = report.header.ok;
    send_report_inner(handle, ctx, recipient, conv, &report, "ReportStableLend").await?;
    info!(?conv, ok, "assign report sent");
    Ok(())
}

async fn send_report_withdraw(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    recipient: [u8; 32],
    conv: [u8; 16],
    report: ReportStableWithdraw,
) -> Result<()> {
    let ok = report.header.ok;
    send_report_inner(
        handle,
        ctx,
        recipient,
        conv,
        &report,
        "ReportStableWithdraw",
    )
    .await?;
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
    let signing_key = ed25519_dalek::SigningKey::from_bytes(ctx.role_identity.signing_key_bytes());
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

#[cfg(test)]
mod sender_allowlist_tests {
    //! Audit-fix C1: the execution daemon must reject Assign / Withdraw
    //! envelopes from any peer other than the configured orchestrator.
    use super::sender_is_authorised;

    const ORCH: [u8; 32] = [7u8; 32];
    const OTHER: [u8; 32] = [9u8; 32];

    #[test]
    fn no_orchestrator_configured_allows_any_sender() {
        // Devnet sandbox: every sender passes when allowlist is disabled.
        assert!(sender_is_authorised(None, OTHER, "Assign"));
        assert!(sender_is_authorised(None, [0u8; 32], "Withdraw"));
    }

    #[test]
    fn matching_sender_is_authorised() {
        assert!(sender_is_authorised(Some(ORCH), ORCH, "Assign"));
        assert!(sender_is_authorised(Some(ORCH), ORCH, "Withdraw"));
    }

    #[test]
    fn mismatched_sender_is_rejected() {
        // The C1 negative case: an Assign / Withdraw from a different
        // peer must be rejected. Caller drops it silently.
        assert!(!sender_is_authorised(Some(ORCH), OTHER, "Assign"));
        assert!(!sender_is_authorised(Some(ORCH), [0u8; 32], "Withdraw"));
    }
}

#[cfg(test)]
mod payload_filter_tests {
    //! Fix 3a (2026-05-13): stable-yield-daemon must silently drop
    //! envelopes whose payload doesn't decode as a stable-lend type.
    use super::payload_is_for_this_daemon;
    use zerox1_protocol::envelope::Envelope;
    use zerox1_protocol::fleet::hedgedjlp::AssignHedgedJlp;
    use zerox1_protocol::fleet::stable_lend::{AssignStableLend, WithdrawStableLend};
    use zerox1_protocol::message::MsgType;

    fn make_env(msg_type: MsgType, payload: Vec<u8>) -> Envelope {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]);
        let sender = sk.verifying_key().to_bytes();
        Envelope::build(msg_type, sender, [0u8; 32], 0, 0, [0u8; 16], payload, &sk)
    }

    #[test]
    fn stable_lend_assign_passes() {
        let assign = AssignStableLend {
            market: [1u8; 32],
            reserve: [2u8; 32],
            usdc_lamports: 50_000_000,
            deadline_unix: 0,
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&assign, &mut buf).unwrap();
        assert!(payload_is_for_this_daemon(&make_env(MsgType::Assign, buf)));
    }

    #[test]
    fn hedgedjlp_assign_is_dropped() {
        let assign = AssignHedgedJlp {
            usdc_lamports: 100,
            target_delta_bps: 0,
            max_borrow_rate_bps: 5000,
            deadline_unix: 0,
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&assign, &mut buf).unwrap();
        assert!(!payload_is_for_this_daemon(&make_env(MsgType::Assign, buf)));
    }

    #[test]
    fn stable_lend_withdraw_passes() {
        let w = WithdrawStableLend {
            market: [1u8; 32],
            reserve: [2u8; 32],
            usdc_lamports: u64::MAX,
            deadline_unix: 0,
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&w, &mut buf).unwrap();
        assert!(payload_is_for_this_daemon(&make_env(
            MsgType::Withdraw,
            buf
        )));
    }
}
