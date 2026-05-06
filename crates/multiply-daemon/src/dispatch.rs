//! Inbox dispatch — decode AssignMultiply, validate against caps,
//! call leverage::run_or_simulate, build ReportMultiply, sign + send.

use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};
use zerox1_defi_runtime::{identity::RoleIdentity, rpc::RpcContext};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::envelope::Envelope;
use zerox1_protocol::fleet::multiply::{AssignMultiply, ReportMultiply};
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
    if escalate.severity == RiskSeverity::Critical
        && escalate.kind == RiskKind::LiquidationDistance
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

/// Receive envelopes; dispatch on MsgType::Assign with an
/// AssignMultiply CBOR payload.
pub async fn run(mut handle: NodeHandle, ctx: DispatchCtx) -> Result<()> {
    while let Some(env) = handle.recv().await {
        match env.msg_type {
            MsgType::Assign => {
                let conv = env.conversation_id;
                let recipient = env.sender;
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
        let added = ctx.approval_queue.enqueue(conv, payload.clone(), env.sender);
        if !added {
            return Err(anyhow!("approval queue full (cap 64); rejecting Assign"));
        }
        // Best-effort emit of the "needs approval" Escalate envelope.
        if let Err(e) = emit_needs_approval(handle, ctx, env).await {
            warn!(?e, ?conv, "failed to emit NeedsApproval Escalate; Assign still queued");
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

/// Build + send an Escalate envelope of kind `NeedsApproval`, routed back
/// to the orchestrator that issued the Assign. Re-uses the Assign's
/// conversation_id so the orchestrator can correlate.
async fn emit_needs_approval(
    handle: &NodeHandle,
    ctx: &DispatchCtx,
    env: &Envelope,
) -> Result<()> {
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
