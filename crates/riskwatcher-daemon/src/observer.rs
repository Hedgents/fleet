//! M3 inbox observer: subscribes to envelopes coming off the embedded
//! `NodeService`, decodes `MsgType::Report` payloads as `ReportMultiply`,
//! and upserts a [`PositionView`] into [`ObservedPositions`] for each
//! finished leverage rebalance the multiply daemon reports.
//!
//! Scope (M3):
//!   - decode multiply Reports only; other-desk Reports decode-fail and
//!     are dropped at debug! (no panic, no escalate)
//!   - ignore queued-ack Reports (`ok=true && resulting_ltv_bps == 0`)
//!   - ignore failed Reports (`ok=false`) — out of scope for the
//!     registry, will be revisited under M5/M6
//!   - log non-Report msg_types at TRACE
//!
//! `obligation_pubkey` is stubbed to [`Pubkey::default()`] (all zeros)
//! because `ReportMultiply` does not carry it. M4 (poller) derives the
//! real Kamino obligation PDA from the subject + lending market on first
//! poll and overwrites this field via `upsert`.

use std::sync::Arc;

use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info, trace, warn};

use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::envelope::Envelope;
use zerox1_protocol::fleet::multiply::ReportMultiply;
use zerox1_protocol::message::MsgType;

use crate::state::{ObservedPositions, PositionView, Source};

/// Pure decision function: given an envelope, decide whether it
/// translates into a [`PositionView`] to upsert.
///
/// Returns `Some(view)` only when:
///   - the envelope's `msg_type` is `Report`
///   - the payload decodes as `ReportMultiply`
///   - `header.ok == true`
///   - `resulting_ltv_bps > 0` (a queued-ack carries `resulting_ltv_bps == 0`)
///
/// All decode failures and non-applicable Reports collapse to `None`,
/// so the caller does not need to know which sub-rule fired. Logging
/// is emitted at the appropriate level here so the live observer loop
/// stays a thin shell.
pub fn handle_report_envelope(env: &Envelope) -> Option<PositionView> {
    if env.msg_type != MsgType::Report {
        trace!(msg_type = ?env.msg_type, "non-Report envelope ignored");
        return None;
    }

    let report: ReportMultiply = match ciborium::de::from_reader(&env.payload[..]) {
        Ok(r) => r,
        Err(e) => {
            // Could be a Report from a different desk (StableFloor,
            // HedgedJlp, StableLend) whose payload shape doesn't match
            // ReportMultiply. M3 is multiply-only — log and drop.
            debug!(
                sender = %hex::encode(env.sender),
                conv = %hex::encode(env.conversation_id),
                error = %e,
                "Report payload not a ReportMultiply; dropping",
            );
            return None;
        }
    };

    if !report.header.ok {
        debug!(
            sender = %hex::encode(env.sender),
            conv = %hex::encode(report.header.conversation_id),
            error_code = report.header.error_code.unwrap_or_default(),
            "ReportMultiply ok=false; out of scope for M3 registry",
        );
        return None;
    }

    if report.resulting_ltv_bps == 0 {
        // Queued-ack: multiply has accepted the Assign but not yet
        // executed the rebalance. There's nothing to track yet.
        debug!(
            sender = %hex::encode(env.sender),
            conv = %hex::encode(report.header.conversation_id),
            "ReportMultiply queued-ack (resulting_ltv_bps=0); ignored",
        );
        return None;
    }

    info!(
        subject = %hex::encode(env.sender),
        conv = %hex::encode(report.header.conversation_id),
        last_ltv_bps = report.resulting_ltv_bps,
        tx_signature = %report.tx_signature.as_deref().unwrap_or("-"),
        last_seen_unix = env.block_ref,
        "Report observed",
    );

    Some(PositionView {
        subject: env.sender,
        // TODO(M4): poller derives the real Kamino obligation PDA from
        // the subject + lending market on first refresh and overwrites
        // this stub via ObservedPositions::upsert.
        obligation_pubkey: Pubkey::default(),
        last_ltv_bps: report.resulting_ltv_bps,
        last_seen_unix: env.block_ref,
        source: Source::Report,
    })
}

/// Drain the inbound envelope stream forever, dispatching Report
/// envelopes through [`handle_report_envelope`] and upserting the
/// resulting view into the shared registry.
pub async fn run(mut handle: NodeHandle, state: Arc<ObservedPositions>) -> Result<()> {
    while let Some(env) = handle.recv().await {
        if let Some(view) = handle_report_envelope(&env) {
            state.upsert(view).await;
            let size = state.len().await;
            info!(size, "registry updated");
        }
    }
    warn!("inbox channel closed; observer exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn dummy_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn build_report_envelope(report: &ReportMultiply, block_ref: u64) -> Envelope {
        let key = dummy_signing_key();
        let sender = key.verifying_key().to_bytes();
        let mut payload = Vec::new();
        ciborium::ser::into_writer(report, &mut payload).expect("serialize ReportMultiply");
        Envelope::build(
            MsgType::Report,
            sender,
            [0u8; 32],
            block_ref,
            1,
            [0u8; 16],
            payload,
            &key,
        )
    }

    #[test]
    fn ok_with_nonzero_ltv_yields_view() {
        let report = ReportMultiply {
            header: zerox1_protocol::fleet::ReportHeader {
                conversation_id: [1u8; 16],
                ok: true,
                error_code: None,
            },
            resulting_ltv_bps: 5500,
            tx_signature: Some("sig".into()),
        };
        let env = build_report_envelope(&report, 1_700_000_000);
        let view = handle_report_envelope(&env).expect("expected Some(view)");
        assert_eq!(view.subject, env.sender);
        assert_eq!(view.last_ltv_bps, 5500);
        assert_eq!(view.last_seen_unix, 1_700_000_000);
        assert_eq!(view.source, Source::Report);
        assert_eq!(view.obligation_pubkey, Pubkey::default());
    }

    #[test]
    fn ok_with_zero_ltv_is_queued_ack_ignored() {
        let report = ReportMultiply {
            header: zerox1_protocol::fleet::ReportHeader {
                conversation_id: [2u8; 16],
                ok: true,
                error_code: None,
            },
            resulting_ltv_bps: 0,
            tx_signature: None,
        };
        let env = build_report_envelope(&report, 0);
        assert!(handle_report_envelope(&env).is_none());
    }

    #[test]
    fn failed_report_ignored() {
        let report = ReportMultiply {
            header: zerox1_protocol::fleet::ReportHeader {
                conversation_id: [3u8; 16],
                ok: false,
                error_code: Some(42),
            },
            resulting_ltv_bps: 5500,
            tx_signature: None,
        };
        let env = build_report_envelope(&report, 0);
        assert!(handle_report_envelope(&env).is_none());
    }

    #[test]
    fn non_report_msg_type_ignored() {
        let key = dummy_signing_key();
        let sender = key.verifying_key().to_bytes();
        let env = Envelope::build(
            MsgType::Beacon,
            sender,
            [0u8; 32],
            0,
            1,
            [0u8; 16],
            Vec::new(),
            &key,
        );
        assert!(handle_report_envelope(&env).is_none());
    }

    #[test]
    fn report_with_undecodable_payload_is_dropped() {
        let key = dummy_signing_key();
        let sender = key.verifying_key().to_bytes();
        // garbage bytes that won't decode as ReportMultiply
        let env = Envelope::build(
            MsgType::Report,
            sender,
            [0u8; 32],
            0,
            1,
            [0u8; 16],
            vec![0xff, 0x00, 0xde, 0xad],
            &key,
        );
        assert!(handle_report_envelope(&env).is_none());
    }
}
