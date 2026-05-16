//! Inbox dispatch — decode AssignMultiply, validate against caps,
//! call leverage::run_or_simulate, build ReportMultiply, sign + send.

use anyhow::{anyhow, Context, Result};
use solana_sdk::pubkey::Pubkey;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};
use zerox1_defi_runtime::{identity::RoleIdentity, rpc::RpcContext};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::envelope::Envelope;
use zerox1_protocol::fleet::multiply::{
    AssignMultiply, ReportMultiply, ReportMultiplyWithdraw, WithdrawMultiply,
};
use zerox1_protocol::fleet::riskwatcher::{EscalateRisk, RiskKind, RiskSeverity};
use zerox1_protocol::fleet::ReportHeader;
use zerox1_protocol::message::MsgType;

use crate::caps;

/// Soft-veto pause duration when a Critical+LiquidationDistance Escalate
/// arrives from the trusted riskwatcher. Multiply refuses new AssignMultiply
/// for this many seconds, then auto-clears on the next inbound Assign.
pub const PAUSE_DURATION_SECS: u64 = 300;

/// error_code returned in ReportMultiply when an Assign is rejected because
/// the daemon is in a riskwatcher-imposed pause window.
pub const ERR_PAUSED_BY_RISKWATCHER: u32 = 4;

/// (Reserved historic slot — error_code 99 was the commit-3 stub value
/// ERR_WITHDRAW_NOT_YET_IMPLEMENTED. Real per-phase codes from §4.3 of
/// the plan are now defined alongside their owners — e.g.
/// `unwind::ERR_JUPITER_INTEGRATION_PENDING = 11`.)

pub struct DispatchCtx {
    pub rpc: Arc<RpcContext>,
    pub wallet: Arc<Wallet>,
    /// Audit-fix I1: SigningWhitelist is now wired into the leverage loop;
    /// every ixn slice passes through `whitelist.verify_ixns` before signing.
    pub whitelist: Arc<SigningWhitelist>,
    pub role_identity: RoleIdentity,
    pub simulate_only: bool,
    pub require_approval: bool,
    pub nonce: Arc<std::sync::atomic::AtomicU64>,
    /// Per-CLI ceiling on collateral the daemon will operate. The leverage
    /// loop uses this to size each round's borrow.
    pub args_max_position_usdc_lamports: u64,
    /// M8: pending-approval queue. When `require_approval=true`, incoming
    /// Assigns land here and wait for a matching Approve envelope.
    pub approval_queue: Arc<crate::approval::ApprovalQueue>,
    /// v0.3.0: parallel approval queue for WithdrawMultiply. Same TTL +
    /// cap as the Assign queue; kept distinct so an Approve resolves
    /// against the correct queued payload type (commit 5 of the
    /// multiply-unwind plan).
    pub withdraw_approval_queue: Arc<crate::approval::WithdrawApprovalQueue>,
    /// v0.3.0 emergency-withdraw escape hatch: when set, future versions
    /// will route unwound USDC to this pubkey instead of leaving it in
    /// the daemon's wallet. v0.3.0 plumbs the flag but does not yet wire
    /// the redirection (which lands alongside the Jupiter swap-to-USDC
    /// adapter in v0.3.1).
    #[allow(dead_code)]
    pub emergency_destination: Option<Pubkey>,
    /// Audit-fix C1: 32-byte pubkey of the orchestrator authorised to send
    /// Assign / Approve envelopes. Required on mainnet (enforced in main.rs).
    /// When `None` (devnet sandbox), the sender allowlist is disabled and
    /// any peer may issue Assigns — preserves the existing paper-trade-loop
    /// behaviour. The check, when active, follows the same loud-warn +
    /// silent-drop shape as the Approve sender mismatch below.
    pub orchestrator_agent_id: Option<[u8; 32]>,
    /// riskwatcher M7: 32-byte pubkey of the trusted riskwatcher daemon.
    /// `None` disables soft-veto entirely (Escalate envelopes are observed
    /// only). When set, a Critical+LiquidationDistance Escalate from this
    /// pubkey triggers a `PAUSE_DURATION_SECS` pause; Escalates from any
    /// other sender are ignored.
    pub riskwatcher_pubkey: Option<[u8; 32]>,
    /// riskwatcher M7: when `Some(t)`, multiply refuses new AssignMultiply
    /// until `now_unix_secs() >= t`. Self-cleared by `is_paused` when the
    /// window expires (no background task). Reset by the Escalate handler.
    pub paused_until_unix: Arc<std::sync::Mutex<Option<u64>>>,
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

/// Wall-clock seconds since UNIX epoch; clamps to 0 on the impossible
/// "system clock before 1970" path so callers never panic.
fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Returns `true` iff the pause window stored at `pause` is still in the
/// future relative to `now`. Self-clearing: an expired pause is reset to
/// `None` so subsequent calls short-circuit through the `None` arm.
///
/// Takes `&Mutex<Option<u64>>` rather than `&DispatchCtx` so unit tests
/// can exercise the state transitions without instantiating the full
/// dispatch context (which carries an RPC client and a Wallet).
fn is_paused_at(pause: &std::sync::Mutex<Option<u64>>, now: u64) -> bool {
    let mut guard = pause.lock().expect("paused_until_unix mutex poisoned");
    match *guard {
        Some(until) if now < until => true,
        Some(_) => {
            *guard = None;
            false
        }
        None => false,
    }
}

/// Wall-clock-driven wrapper around [`is_paused_at`]. Production callers
/// use this; tests use the `_at` variant with a controlled `now`.
fn is_paused(ctx: &DispatchCtx) -> bool {
    is_paused_at(&ctx.paused_until_unix, now_unix_secs())
}

/// Apply an `EscalateRisk` to the pause state stored at `pause`. Returns
/// `Some(until)` if a pause was set (severity Critical + kind
/// LiquidationDistance), `None` otherwise. The only side effect is the
/// mutex write; extracted for unit testing.
fn apply_escalate_to(
    pause: &std::sync::Mutex<Option<u64>>,
    escalate: &EscalateRisk,
    now: u64,
) -> Option<u64> {
    if escalate.severity == RiskSeverity::Critical && escalate.kind == RiskKind::LiquidationDistance
    {
        let until = now + PAUSE_DURATION_SECS;
        let mut guard = pause.lock().expect("paused_until_unix mutex poisoned");
        *guard = Some(until);
        Some(until)
    } else {
        None
    }
}

/// Wall-clock-driven wrapper around [`apply_escalate_to`].
fn apply_escalate(ctx: &DispatchCtx, escalate: &EscalateRisk, now: u64) -> Option<u64> {
    apply_escalate_to(&ctx.paused_until_unix, escalate, now)
}

/// Returns `true` if the envelope's payload cleanly CBOR-decodes as an
/// AssignMultiply (the only Assign type this daemon owns). Defence-in-depth
/// (Fix 3a, 2026-05-13): silently drop unrelated payloads so the daemon
/// doesn't send error Reports for Assigns aimed at other desks.
fn payload_is_for_this_daemon(env: &Envelope) -> bool {
    match env.msg_type {
        MsgType::Assign => ciborium::de::from_reader::<AssignMultiply, _>(&env.payload[..]).is_ok(),
        MsgType::WithdrawMultiply => {
            ciborium::de::from_reader::<WithdrawMultiply, _>(&env.payload[..]).is_ok()
        }
        _ => true,
    }
}

/// Receive envelopes; dispatch on MsgType::Assign with an
/// AssignMultiply CBOR payload.
pub async fn run(mut handle: NodeHandle, ctx: DispatchCtx) -> Result<()> {
    while let Some(env) = handle.recv().await {
        if !payload_is_for_this_daemon(&env) {
            debug!(
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
                // Audit-fix C1: sender allowlist. Silent drop — same shape
                // as the Approve mismatch handler below — so a probing
                // attacker gets no signal back.
                if !sender_is_authorised(ctx.orchestrator_agent_id, env.sender, "Assign") {
                    continue;
                }
                // riskwatcher M7 soft-veto: check the pause window BEFORE
                // any cap validation or leverage execution. If paused,
                // reject with error_code=4 and route the Report back to
                // the Assign sender (the orchestrator).
                if is_paused(&ctx) {
                    warn!(?conv, "Assign rejected — paused by riskwatcher veto");
                    let report = ReportMultiply {
                        header: ReportHeader::err(conv, ERR_PAUSED_BY_RISKWATCHER),
                        resulting_ltv_bps: 0,
                        tx_signature: None,
                    };
                    let _ = send_report(&handle, &ctx, recipient, conv, report).await;
                    continue;
                }
                match handle_assign(&handle, &ctx, &env).await {
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
            MsgType::Approve => {
                let conv = env.conversation_id;
                let recipient = env.sender;
                // riskwatcher M7 soft-veto (extension): a queued Assign
                // approved AFTER a pause landed must also be rejected.
                // This goes one step beyond the strict M7 spec but is the
                // right safety behaviour — without it, an in-flight queued
                // Assign would slip past the veto.
                if is_paused(&ctx) {
                    warn!(?conv, "Approve rejected — paused by riskwatcher veto");
                    let report = ReportMultiply {
                        header: ReportHeader::err(conv, ERR_PAUSED_BY_RISKWATCHER),
                        resulting_ltv_bps: 0,
                        tx_signature: None,
                    };
                    let _ = send_report(&handle, &ctx, recipient, conv, report).await;
                    continue;
                }
                match ctx.approval_queue.approve(conv, env.sender) {
                    crate::approval::ApproveResult::Approved(payload) => {
                        info!(?conv, "Approve received — executing queued AssignMultiply");
                        // Audit-fix I2: defense in depth — re-validate caps even
                        // though we validated on enqueue. Caps are compile-time
                        // constants so this is belt-and-suspenders, but cheap.
                        if let Err(e) = caps::validate_assign(&payload) {
                            warn!(?e, ?conv, "post-approval cap re-validation failed");
                            let report = ReportMultiply {
                                header: ReportHeader::err(conv, 3),
                                resulting_ltv_bps: 0,
                                tx_signature: None,
                            };
                            let _ = send_report(&handle, &ctx, recipient, conv, report).await;
                            continue;
                        }
                        match crate::leverage::run_or_simulate(&ctx, &payload, conv).await {
                            Ok(report) => {
                                let _ = send_report(&handle, &ctx, recipient, conv, report).await;
                            }
                            Err(e) => {
                                warn!(?e, ?conv, "queued assign failed; sending error Report");
                                let report = ReportMultiply {
                                    header: ReportHeader::err(conv, 2),
                                    resulting_ltv_bps: 0,
                                    tx_signature: None,
                                };
                                let _ = send_report(&handle, &ctx, recipient, conv, report).await;
                            }
                        }
                    }
                    crate::approval::ApproveResult::NotFound => {
                        // No matching Assign — but the Approve might be for a
                        // queued WithdrawMultiply. Probe the withdraw queue
                        // before giving up. The two queues are keyed by the
                        // same `conv_id` namespace; orchestrators are
                        // responsible for choosing distinct conv_ids for
                        // Assign vs Withdraw (the fleet-pm-stub does this).
                        match ctx.withdraw_approval_queue.approve(conv, env.sender) {
                            crate::approval::WithdrawApproveResult::Approved(w_payload) => {
                                info!(
                                    ?conv,
                                    "Approve received — executing queued WithdrawMultiply"
                                );
                                if let Err(e) = caps::validate_withdraw_multiply(&w_payload) {
                                    warn!(
                                        ?e,
                                        ?conv,
                                        "post-approval withdraw cap re-validation failed"
                                    );
                                    let report = ReportMultiplyWithdraw {
                                        header: ReportHeader::err(conv, 2),
                                        final_usdc_lamports: 0,
                                        residual_sol_lamports: 0,
                                        tx_signatures: vec![],
                                    };
                                    let _ = send_report_withdraw(
                                        &handle, &ctx, recipient, conv, report,
                                    )
                                    .await;
                                    continue;
                                }
                                match crate::unwind::run_or_simulate(&ctx, &w_payload, conv).await {
                                    Ok(report) => {
                                        let _ = send_report_withdraw(
                                            &handle, &ctx, recipient, conv, report,
                                        )
                                        .await;
                                    }
                                    Err(e) => {
                                        warn!(
                                            ?e,
                                            ?conv,
                                            "queued withdraw failed; sending error Report"
                                        );
                                        let report = ReportMultiplyWithdraw {
                                            header: ReportHeader::err(conv, 1),
                                            final_usdc_lamports: 0,
                                            residual_sol_lamports: 0,
                                            tx_signatures: vec![],
                                        };
                                        let _ = send_report_withdraw(
                                            &handle, &ctx, recipient, conv, report,
                                        )
                                        .await;
                                    }
                                }
                            }
                            crate::approval::WithdrawApproveResult::NotFound => {
                                warn!(
                                    ?conv,
                                    "Approve received but no matching pending Assign / \
                                     WithdrawMultiply (or expired)"
                                );
                            }
                            crate::approval::WithdrawApproveResult::SenderMismatch {
                                expected,
                                got,
                            } => {
                                warn!(
                                    ?conv,
                                    expected = %hex::encode(expected),
                                    got = %hex::encode(got),
                                    "Approve REJECTED — sender does not match the original \
                                     WithdrawMultiply sender."
                                );
                            }
                        }
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
            MsgType::Escalate => {
                // riskwatcher M7: soft-veto. Authorisation comes first —
                // if no riskwatcher is configured, OR the sender doesn't
                // match the configured pubkey, drop the envelope silently.
                let Some(expected_pubkey) = ctx.riskwatcher_pubkey else {
                    debug!("Escalate received but no --riskwatcher configured; ignoring");
                    continue;
                };
                if env.sender != expected_pubkey {
                    warn!(
                        sender = %hex::encode(env.sender),
                        expected = %hex::encode(expected_pubkey),
                        "Escalate REJECTED — sender does not match configured riskwatcher pubkey"
                    );
                    continue;
                }

                let escalate: EscalateRisk = match ciborium::de::from_reader(&env.payload[..]) {
                    Ok(e) => e,
                    Err(e) => {
                        warn!(?e, "Escalate payload decode failed");
                        continue;
                    }
                };

                match apply_escalate(&ctx, &escalate, now_unix_secs()) {
                    Some(until) => info!(
                        until,
                        subject = %hex::encode(escalate.subject),
                        "PAUSED by riskwatcher veto for {}s",
                        PAUSE_DURATION_SECS,
                    ),
                    None => debug!(
                        severity = ?escalate.severity,
                        kind = ?escalate.kind,
                        "Escalate received — non-pause-triggering, observed only",
                    ),
                }
            }
            MsgType::WithdrawMultiply => {
                let conv = env.conversation_id;
                let recipient = env.sender;
                // Same sender-allowlist + pause gates as Assign — withdraw is
                // an authority-shaped action.
                if !sender_is_authorised(ctx.orchestrator_agent_id, env.sender, "WithdrawMultiply")
                {
                    continue;
                }
                if is_paused(&ctx) {
                    warn!(
                        ?conv,
                        "WithdrawMultiply rejected — paused by riskwatcher veto"
                    );
                    let report = ReportMultiplyWithdraw {
                        header: ReportHeader::err(conv, ERR_PAUSED_BY_RISKWATCHER),
                        final_usdc_lamports: 0,
                        residual_sol_lamports: 0,
                        tx_signatures: vec![],
                    };
                    let _ = send_report_withdraw(&handle, &ctx, recipient, conv, report).await;
                    continue;
                }
                match handle_withdraw(&handle, &ctx, &env).await {
                    Ok(report) => {
                        let _ = send_report_withdraw(&handle, &ctx, recipient, conv, report).await;
                    }
                    Err(e) => {
                        warn!(?e, ?conv, "withdraw failed; sending error Report");
                        let report = ReportMultiplyWithdraw {
                            header: ReportHeader::err(conv, 1),
                            final_usdc_lamports: 0,
                            residual_sol_lamports: 0,
                            tx_signatures: vec![],
                        };
                        let _ = send_report_withdraw(&handle, &ctx, recipient, conv, report).await;
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

/// Bilateral send of a `ReportMultiplyWithdraw` envelope back to the
/// orchestrator that issued the `WithdrawMultiply`. Mirrors
/// [`send_report`] but for the withdraw report shape. CC's the
/// configured riskwatcher so it sees the unwind outcome too.
async fn send_report_withdraw(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    recipient: [u8; 32],
    conv: [u8; 16],
    report: ReportMultiplyWithdraw,
) -> Result<()> {
    let signing_key = ed25519_dalek::SigningKey::from_bytes(ctx.role_identity.signing_key_bytes());
    let sender_pubkey = signing_key.verifying_key().to_bytes();

    let mut payload = Vec::new();
    ciborium::ser::into_writer(&report, &mut payload)
        .context("serialize ReportMultiplyWithdraw")?;

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let nonce = ctx.nonce.fetch_add(1, Ordering::SeqCst);

    let env = Envelope::build(
        MsgType::Report,
        sender_pubkey,
        recipient,
        now_secs,
        nonce,
        conv,
        payload.clone(),
        &signing_key,
    );
    handle.send(env).await.context("send Report (withdraw)")?;
    info!(?conv, ok = report.header.ok, "withdraw report sent");

    // riskwatcher CC — same shape as send_report.
    if let Some(rw_pubkey) = ctx.riskwatcher_pubkey {
        if rw_pubkey != recipient {
            let cc_nonce = ctx.nonce.fetch_add(1, Ordering::SeqCst);
            let cc_env = Envelope::build(
                MsgType::Report,
                sender_pubkey,
                rw_pubkey,
                now_secs,
                cc_nonce,
                conv,
                payload,
                &signing_key,
            );
            match handle.send(cc_env).await {
                Ok(()) => debug!(
                    ?conv,
                    rw = %hex::encode(rw_pubkey),
                    "withdraw Report CC'd to riskwatcher"
                ),
                Err(e) => warn!(
                    ?e,
                    ?conv,
                    rw = %hex::encode(rw_pubkey),
                    "withdraw Report CC to riskwatcher failed (non-fatal)"
                ),
            }
        }
    }
    Ok(())
}

async fn handle_assign(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    env: &Envelope,
) -> Result<ReportMultiply> {
    let payload: AssignMultiply = ciborium::de::from_reader(&env.payload[..])
        .context("decode AssignMultiply CBOR payload")?;

    info!(
        target_ltv_bps = payload.target_ltv_bps,
        max_slippage_bps = payload.max_slippage_bps,
        "AssignMultiply received"
    );

    // Cap validation — refuses values above hard caps regardless of orchestrator.
    caps::validate_assign(&payload).context("cap validation")?;

    // Approval gate. M8: when require_approval is true, queue the Assign
    // and emit Escalate(Notice, NeedsApproval) to the orchestrator.
    if ctx.require_approval {
        let conv = env.conversation_id;
        info!(?conv, "AssignMultiply queued — awaiting Approve");
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
        // Return an "ok=true" Report with resulting_ltv_bps=0 + tx_signature=None
        // to acknowledge the Assign was received and queued.
        return Ok(ReportMultiply {
            header: ReportHeader::ok(conv),
            resulting_ltv_bps: 0,
            tx_signature: None,
        });
    }

    let conv = env.conversation_id;
    crate::leverage::run_or_simulate(ctx, &payload, conv).await
}

/// Handle an inbound WithdrawMultiply envelope. Decodes, cap-validates,
/// deadline-checks, then either queues (if `require_approval`) or
/// executes immediately via [`crate::unwind::run_or_simulate`].
async fn handle_withdraw(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    env: &Envelope,
) -> Result<ReportMultiplyWithdraw> {
    let payload: WithdrawMultiply = ciborium::de::from_reader(&env.payload[..])
        .context("decode WithdrawMultiply CBOR payload")?;

    info!(
        max_slippage_bps = payload.max_slippage_bps,
        deadline_unix = payload.deadline_unix,
        "WithdrawMultiply received"
    );

    caps::validate_withdraw_multiply(&payload).context("withdraw cap validation")?;

    // Deadline gate before any chain work.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if payload.deadline_unix > 0 && payload.deadline_unix < now {
        let conv = env.conversation_id;
        return Ok(ReportMultiplyWithdraw {
            header: ReportHeader::err(conv, 3),
            final_usdc_lamports: 0,
            residual_sol_lamports: 0,
            tx_signatures: vec![],
        });
    }

    // Approval-queue path (mainnet default). Queue + emit NeedsApproval
    // Escalate; the actual unwind runs when a matching Approve arrives.
    if ctx.require_approval {
        let conv = env.conversation_id;
        info!(?conv, "WithdrawMultiply queued — awaiting Approve");
        let added = ctx
            .withdraw_approval_queue
            .enqueue(conv, payload.clone(), env.sender);
        if !added {
            return Err(anyhow!(
                "withdraw approval queue full (cap 64); rejecting WithdrawMultiply"
            ));
        }
        if let Err(e) = emit_needs_approval(handle, ctx, env).await {
            warn!(
                ?e,
                ?conv,
                "failed to emit NeedsApproval Escalate for WithdrawMultiply; queued anyway"
            );
        }
        return Ok(ReportMultiplyWithdraw {
            header: ReportHeader::ok(conv),
            final_usdc_lamports: 0,
            residual_sol_lamports: 0,
            tx_signatures: vec![],
        });
    }

    // Devnet sandbox path: execute immediately.
    let conv = env.conversation_id;
    crate::unwind::run_or_simulate(ctx, &payload, conv).await
}

/// Build + send an Escalate envelope of kind `NeedsApproval`, routed back
/// to the orchestrator that issued the Assign. Re-uses the Assign's
/// conversation_id so the orchestrator can correlate.
async fn emit_needs_approval(handle: &NodeHandle, ctx: &DispatchCtx, env: &Envelope) -> Result<()> {
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

async fn send_report(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    recipient: [u8; 32],
    conv: [u8; 16],
    report: ReportMultiply,
) -> Result<()> {
    let signing_key = ed25519_dalek::SigningKey::from_bytes(ctx.role_identity.signing_key_bytes());
    let sender_pubkey = signing_key.verifying_key().to_bytes();

    let mut payload = Vec::new();
    ciborium::ser::into_writer(&report, &mut payload).context("serialize ReportMultiply")?;

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
        payload.clone(),
        &signing_key,
    );
    handle.send(env).await.context("send Report")?;
    info!(?conv, ok = report.header.ok, "report sent");

    // riskwatcher M8: CC the Report to the configured riskwatcher so it
    // can populate its observed-positions registry. ReportMultiply is
    // bilateral (recipient = orchestrator/Assign sender); without this
    // CC the third-peer riskwatcher never sees a Report through the
    // mesh. Best-effort: a failed CC is logged at warn but never fatal.
    //
    // Skip the CC when the orchestrator IS the riskwatcher (would be
    // duplicate work for the same recipient).
    if let Some(rw_pubkey) = ctx.riskwatcher_pubkey {
        if rw_pubkey != recipient {
            let cc_nonce = ctx.nonce.fetch_add(1, Ordering::SeqCst);
            let cc_env = Envelope::build(
                MsgType::Report,
                sender_pubkey,
                rw_pubkey,
                now_secs,
                cc_nonce,
                conv,
                payload,
                &signing_key,
            );
            match handle.send(cc_env).await {
                Ok(()) => debug!(
                    ?conv,
                    rw = %hex::encode(rw_pubkey),
                    "Report CC'd to riskwatcher"
                ),
                Err(e) => warn!(
                    ?e,
                    ?conv,
                    rw = %hex::encode(rw_pubkey),
                    "Report CC to riskwatcher failed (non-fatal)"
                ),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod pause_tests {
    use super::*;
    use std::sync::Mutex;

    fn fresh_pause() -> Mutex<Option<u64>> {
        Mutex::new(None)
    }

    #[test]
    fn is_paused_with_no_value_returns_false() {
        let pause = fresh_pause();
        assert!(!is_paused_at(&pause, 1_000));
        // Still None after the call.
        assert_eq!(*pause.lock().unwrap(), None);
    }

    #[test]
    fn is_paused_with_future_value_returns_true() {
        let pause = Mutex::new(Some(2_000));
        assert!(is_paused_at(&pause, 1_000));
        // Not cleared while the window is open.
        assert_eq!(*pause.lock().unwrap(), Some(2_000));
    }

    #[test]
    fn is_paused_with_past_value_clears_and_returns_false() {
        let pause = Mutex::new(Some(500));
        assert!(!is_paused_at(&pause, 1_000));
        // Self-cleaning: expired pause is reset to None.
        assert_eq!(*pause.lock().unwrap(), None);
    }

    #[test]
    fn is_paused_at_exact_boundary_returns_false() {
        // until == now means the window has just expired (strict <).
        let pause = Mutex::new(Some(1_000));
        assert!(!is_paused_at(&pause, 1_000));
        assert_eq!(*pause.lock().unwrap(), None);
    }

    #[test]
    fn apply_escalate_to_critical_liquidation_sets_pause() {
        let pause = fresh_pause();
        let escalate = EscalateRisk {
            severity: RiskSeverity::Critical,
            kind: RiskKind::LiquidationDistance,
            subject: [7u8; 32],
            measurement: 0,
            raised_at_unix: 100,
        };
        let until = apply_escalate_to(&pause, &escalate, 1_000)
            .expect("Critical+LiquidationDistance must set the pause");
        assert_eq!(until, 1_000 + PAUSE_DURATION_SECS);
        assert_eq!(*pause.lock().unwrap(), Some(until));
    }

    #[test]
    fn apply_escalate_to_warning_severity_is_noop() {
        let pause = fresh_pause();
        let escalate = EscalateRisk {
            severity: RiskSeverity::Warning,
            kind: RiskKind::LiquidationDistance,
            subject: [0u8; 32],
            measurement: 0,
            raised_at_unix: 100,
        };
        assert!(apply_escalate_to(&pause, &escalate, 1_000).is_none());
        assert_eq!(*pause.lock().unwrap(), None);
    }

    #[test]
    fn apply_escalate_to_other_kind_is_noop() {
        let pause = fresh_pause();
        let escalate = EscalateRisk {
            severity: RiskSeverity::Critical,
            kind: RiskKind::OracleStaleness,
            subject: [0u8; 32],
            measurement: 0,
            raised_at_unix: 100,
        };
        // Critical is necessary but not sufficient — kind must also match.
        assert!(apply_escalate_to(&pause, &escalate, 1_000).is_none());
        assert_eq!(*pause.lock().unwrap(), None);
    }

    #[test]
    fn pause_default_is_none() {
        // Mirrors how DispatchCtx is constructed in main.rs: a fresh
        // Arc<Mutex<Option<u64>>> seeded with None.
        let pause = Arc::new(Mutex::new(None::<u64>));
        assert_eq!(*pause.lock().unwrap(), None);
        // is_paused on a fresh ctx returns false.
        assert!(!is_paused_at(&pause, 1_000));
    }
}

#[cfg(test)]
mod sender_allowlist_tests {
    //! Audit-fix C1: the execution daemon must reject Assign / Withdraw
    //! envelopes from any peer other than the configured orchestrator.
    //! These tests pin that contract at the helper layer.
    use super::sender_is_authorised;

    const ORCH: [u8; 32] = [7u8; 32];
    const OTHER: [u8; 32] = [9u8; 32];

    #[test]
    fn no_orchestrator_configured_allows_any_sender() {
        // Devnet sandbox: when --orchestrator-agent-id is omitted, the
        // allowlist is disabled and every sender passes. Required so the
        // existing paper-trade-loop keeps working unchanged.
        assert!(sender_is_authorised(None, OTHER, "Assign"));
        assert!(sender_is_authorised(None, [0u8; 32], "Assign"));
    }

    #[test]
    fn matching_sender_is_authorised() {
        assert!(sender_is_authorised(Some(ORCH), ORCH, "Assign"));
    }

    #[test]
    fn mismatched_sender_is_rejected() {
        // The C1 negative case: an Assign from a different peer must be
        // rejected even when one is configured. The caller drops it
        // silently — we just confirm the gate returns false.
        assert!(!sender_is_authorised(Some(ORCH), OTHER, "Assign"));
        assert!(!sender_is_authorised(Some(ORCH), [0u8; 32], "Withdraw"));
    }
}

#[cfg(test)]
mod payload_filter_tests {
    //! Fix 3a (2026-05-13): multiply-daemon must silently drop Assigns
    //! whose payload isn't AssignMultiply.
    use super::payload_is_for_this_daemon;
    use zerox1_protocol::envelope::Envelope;
    use zerox1_protocol::fleet::multiply::{AssignMultiply, WithdrawMultiply};
    use zerox1_protocol::fleet::stable_lend::{AssignStableLend, WithdrawStableLend};
    use zerox1_protocol::message::MsgType;

    fn make_env(msg_type: MsgType, payload: Vec<u8>) -> Envelope {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]);
        let sender = sk.verifying_key().to_bytes();
        Envelope::build(msg_type, sender, [0u8; 32], 0, 0, [0u8; 16], payload, &sk)
    }

    #[test]
    fn assign_multiply_payload_passes() {
        let assign = AssignMultiply {
            vault: [0u8; 32],
            target_ltv_bps: 6000,
            max_slippage_bps: 50,
            deadline_unix: 0,
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&assign, &mut buf).unwrap();
        assert!(payload_is_for_this_daemon(&make_env(MsgType::Assign, buf)));
    }

    #[test]
    fn stable_lend_assign_payload_is_dropped() {
        let assign = AssignStableLend {
            market: [1u8; 32],
            reserve: [2u8; 32],
            usdc_lamports: 50_000_000,
            deadline_unix: 0,
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&assign, &mut buf).unwrap();
        assert!(!payload_is_for_this_daemon(&make_env(MsgType::Assign, buf)));
    }

    #[test]
    fn withdraw_multiply_payload_passes() {
        let withdraw = WithdrawMultiply {
            vault: [3u8; 32],
            max_slippage_bps: 100,
            deadline_unix: 0,
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&withdraw, &mut buf).unwrap();
        assert!(payload_is_for_this_daemon(&make_env(
            MsgType::WithdrawMultiply,
            buf,
        )));
    }

    #[test]
    fn stable_lend_withdraw_on_multiply_slot_is_dropped() {
        // A WithdrawStableLend CBOR payload arriving on the
        // WithdrawMultiply slot must be filtered out so the multiply
        // daemon doesn't accidentally process a stable-yield withdraw.
        // (In practice they ride different slots — 0x18 vs 0x1A — but
        // the type-check is defence in depth at the dispatch layer.)
        let withdraw = WithdrawStableLend {
            market: [1u8; 32],
            reserve: [2u8; 32],
            usdc_lamports: 50_000_000,
            deadline_unix: 0,
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&withdraw, &mut buf).unwrap();
        // WithdrawStableLend has 4 fields where two are 32-byte arrays;
        // WithdrawMultiply has 3 fields where one is a 32-byte array.
        // CBOR map/array shapes differ, so the type-check filters this out.
        assert!(!payload_is_for_this_daemon(&make_env(
            MsgType::WithdrawMultiply,
            buf,
        )));
    }
}
