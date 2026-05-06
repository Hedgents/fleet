//! M6: Escalate envelope emission with `(subject, severity)` de-dup.
//!
//! When the M5 classifier flags a band breach in `poll_one`, riskwatcher
//! signs and sends an [`EscalateRisk`] envelope onto the mesh. This module
//! owns:
//!
//!   * [`emit`] — build CBOR payload, wrap in [`Envelope`], sign with the
//!     role key, and send via [`NodeHandle::send`]. Routing is a function
//!     of severity (Critical → orchestrator + subject; non-Critical →
//!     orchestrator only).
//!   * [`DedupCache`] — async-safe `(subject, severity_as_u8)` →
//!     last-emit-unix-secs map. The cache short-circuits repeat emissions
//!     for the same `(subject, severity)` tuple within
//!     [`DEDUP_WINDOW_SECS`].
//!
//! Failure policy: send errors are logged at `warn!` and swallowed by the
//! caller. The poll loop must never die because the mesh hiccupped.
//!
//! Why u8 keys: `RiskSeverity` derives `PartialEq` only, so we can't
//! `HashMap<(_, RiskSeverity), _>` directly. The repr is `u8`, so the
//! lossless cast `severity as u8` gives a stable hashable key.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use zerox1_defi_runtime::identity::RoleIdentity;
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::envelope::Envelope;
use zerox1_protocol::fleet::riskwatcher::{EscalateRisk, RiskKind, RiskSeverity};
use zerox1_protocol::message::MsgType;

/// Suppress repeat emissions for the same `(subject, severity)` within
/// this many seconds. Hard-coded by spec — not configurable.
pub const DEDUP_WINDOW_SECS: u64 = 60;

/// Async-safe `(subject, severity_u8)` → last-emit-unix-secs.
///
/// Wrapped in `tokio::sync::Mutex` because the poll loop fans out one
/// `poll_one` future per subject and they may race even on a single
/// thread. Lookup is O(1); the map never grows beyond the registry
/// capacity (32) × number of severities (3).
#[derive(Default)]
pub struct DedupCache {
    inner: Mutex<HashMap<([u8; 32], u8), u64>>,
}

impl DedupCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically test-and-set: returns `true` if the caller should emit
    /// an Escalate for `(subject, severity)` at `now_unix`, recording the
    /// timestamp in that case. Returns `false` if a prior emission for
    /// the same key occurred within [`DEDUP_WINDOW_SECS`].
    ///
    /// The dedup key is `(subject, severity)`. Different severities for
    /// the same subject are independent; different subjects are
    /// independent. `kind` is intentionally NOT part of the key.
    pub async fn should_emit(
        &self,
        subject: [u8; 32],
        severity: RiskSeverity,
        now_unix: u64,
    ) -> bool {
        let key = (subject, severity as u8);
        let mut guard = self.inner.lock().await;
        match guard.get(&key) {
            Some(&prev) if now_unix.saturating_sub(prev) <= DEDUP_WINDOW_SECS => false,
            _ => {
                guard.insert(key, now_unix);
                true
            }
        }
    }
}

/// Build, sign, and send a single [`EscalateRisk`] envelope to one
/// recipient.
///
/// This is the low-level emit that does not consult the dedup cache nor
/// duplicate-route Critical to two targets — the higher-level
/// [`emit_classified`] wraps this. Exposed `pub` for testability and so
/// future callers can route Escalates to bespoke recipients without
/// re-implementing the envelope build.
#[allow(clippy::too_many_arguments)]
pub async fn emit(
    handle: &NodeHandle,
    role: &RoleIdentity,
    nonce: &AtomicU64,
    recipient: [u8; 32],
    severity: RiskSeverity,
    kind: RiskKind,
    subject: [u8; 32],
    measurement: i64,
) -> Result<()> {
    let signing_key = ed25519_dalek::SigningKey::from_bytes(role.signing_key_bytes());
    let sender_pubkey = signing_key.verifying_key().to_bytes();

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let escalate = EscalateRisk {
        severity,
        kind,
        subject,
        measurement,
        raised_at_unix: now_secs,
    };

    let mut payload = Vec::new();
    ciborium::ser::into_writer(&escalate, &mut payload).context("serialize EscalateRisk")?;

    let n = nonce.fetch_add(1, Ordering::SeqCst);

    let env = Envelope::build(
        MsgType::Escalate,
        sender_pubkey,
        recipient,
        now_secs,
        n,
        [0u8; 16], // proactive escalates have no conversation
        payload,
        &signing_key,
    );

    handle.send(env).await.context("send EscalateRisk")?;
    info!(
        ?severity,
        ?kind,
        recipient = %hex::encode(recipient),
        subject = %hex::encode(subject),
        nonce = n,
        measurement,
        "Escalate emitted"
    );
    Ok(())
}

/// Apply spec step 4 + 5: dedup-aware emission with severity-driven
/// recipient routing.
///
/// On dedup miss, emits one envelope to `orchestrator`; for Critical, also
/// emits a second envelope routed to `subject` (the multiply-daemon's role
/// pubkey, equal to the subject bytes per the role-identity = wallet
/// model). On dedup hit, suppresses for both recipients.
///
/// Send failures are logged but do not propagate — the caller (poller)
/// must not die because of a transient mesh issue.
#[allow(clippy::too_many_arguments)]
pub async fn emit_classified(
    handle: &NodeHandle,
    role: &RoleIdentity,
    nonce: &AtomicU64,
    dedup: &DedupCache,
    orchestrator: [u8; 32],
    severity: RiskSeverity,
    kind: RiskKind,
    subject: [u8; 32],
    measurement: i64,
) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    if !dedup.should_emit(subject, severity, now).await {
        debug!(
            ?severity,
            subject = %hex::encode(subject),
            "Escalate suppressed by dedup ({}s window)",
            DEDUP_WINDOW_SECS,
        );
        return;
    }

    // Primary recipient: the orchestrator. Always.
    if let Err(e) = emit(handle, role, nonce, orchestrator, severity, kind, subject, measurement)
        .await
    {
        warn!(?e, ?severity, "Escalate to orchestrator failed");
    }

    // Critical also fans out to the position subject (multiply-daemon).
    // Spec step 5 — non-Critical does NOT fan out.
    if matches!(severity, RiskSeverity::Critical) {
        if let Err(e) =
            emit(handle, role, nonce, subject, severity, kind, subject, measurement).await
        {
            warn!(?e, ?severity, "Escalate to subject (multiply) failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[tokio::test]
    async fn dedup_suppresses_within_window() {
        let cache = DedupCache::new();
        let subj = s(1);

        assert!(cache.should_emit(subj, RiskSeverity::Critical, 100).await);
        assert!(!cache.should_emit(subj, RiskSeverity::Critical, 130).await);
        // boundary: 60s exactly is still within window (≤)
        assert!(!cache.should_emit(subj, RiskSeverity::Critical, 160).await);
        // > 60s allows again
        assert!(cache.should_emit(subj, RiskSeverity::Critical, 200).await);
    }

    #[tokio::test]
    async fn dedup_keys_on_severity() {
        let cache = DedupCache::new();
        let subj = s(2);
        // Same subject, different severity → independent keys.
        assert!(cache.should_emit(subj, RiskSeverity::Critical, 100).await);
        assert!(cache.should_emit(subj, RiskSeverity::Warning, 100).await);
        assert!(cache.should_emit(subj, RiskSeverity::Notice, 100).await);
        // And each is now suppressed within its own window.
        assert!(!cache.should_emit(subj, RiskSeverity::Warning, 110).await);
    }

    #[tokio::test]
    async fn dedup_keys_on_subject() {
        let cache = DedupCache::new();
        // Different subjects → independent keys.
        assert!(cache.should_emit(s(3), RiskSeverity::Critical, 100).await);
        assert!(cache.should_emit(s(4), RiskSeverity::Critical, 100).await);
        // Each still suppresses on its own subject.
        assert!(!cache.should_emit(s(3), RiskSeverity::Critical, 105).await);
    }

    /// Smoke (Path A): the dedup cache plus a hand-built call sequence
    /// proves that, given a Critical severity, two recipients are
    /// targeted (orchestrator + subject) and a same-tick repeat is
    /// suppressed for both.
    ///
    /// We test the dedup gate in isolation here — `emit_classified`'s
    /// outbound `handle.send` path needs a `NodeHandle` mock which is
    /// out of scope for M6. The full async path is exercised by M8's
    /// devnet round-trip.
    #[tokio::test]
    async fn dedup_blocks_critical_repeat_within_window() {
        let cache = DedupCache::new();
        let subj = s(7);
        // First Critical at t=100 — emit allowed.
        assert!(cache.should_emit(subj, RiskSeverity::Critical, 100).await);
        // Same Critical at t=105 — suppressed (would block BOTH recipients).
        assert!(!cache.should_emit(subj, RiskSeverity::Critical, 105).await);
    }
}
