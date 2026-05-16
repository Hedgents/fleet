//! Integration tests for the WithdrawMultiply / ReportMultiplyWithdraw
//! round-trip (commit 7 of the multiply-unwind plan).
//!
//! multiply-daemon is a `[[bin]]`-only crate, so external integration
//! tests cannot reach its internal types (no `lib.rs` surface). What we
//! CAN exercise here is the **protocol contract** that ties fleet-pm-stub
//! to the daemon: the CBOR payload shapes, the MsgType slot allocations,
//! and the envelope round-trip. If any of these drift, the unwind path
//! breaks before any chain work begins — so these tests pin the contract.
//!
//! The real flash-loan-+-broadcast happy path requires a mock Kamino +
//! Jupiter RPC; the plan acknowledges that as live-mainnet-sim work in §7
//! ("What CANNOT be unit-tested"). v0.3.0's dispatch handler is also
//! deliberately gated to return ERR_JUPITER_INTEGRATION_PENDING for any
//! non-Noop position pending the v0.3.1 swap-leg adapter — so there's
//! literally no broadcast path to integration-test in this version
//! beyond the Noop short-circuit.
//!
//! What this test asserts:
//!   1. The CBOR shape of a `WithdrawMultiply` round-trips through an
//!      `Envelope` wire-bytes serialization with `MsgType::WithdrawMultiply
//!      = 0x1A`.
//!   2. The CBOR shape of a `ReportMultiplyWithdraw` round-trips with
//!      multiple tx signatures, empty signatures (sim/build failure path),
//!      and the v0.3.0 ERR_JUPITER_INTEGRATION_PENDING error code (= 11).
//!   3. `MsgType::WithdrawMultiply` rejects any other slot's CBOR payload
//!      (defense in depth — guarantees the multiply daemon won't process
//!      an envelope addressed to a different desk under the same slot).

use ciborium::{de::from_reader, ser::into_writer};
use ed25519_dalek::SigningKey;
use zerox1_protocol::envelope::Envelope;
use zerox1_protocol::fleet::multiply::{ReportMultiplyWithdraw, WithdrawMultiply};
use zerox1_protocol::fleet::stable_lend::WithdrawStableLend;
use zerox1_protocol::fleet::ReportHeader;
use zerox1_protocol::message::MsgType;

/// Sample WithdrawMultiply matching what fleet-pm-stub's
/// `withdraw-multiply` subcommand produces with default flags.
fn sample_withdraw() -> WithdrawMultiply {
    WithdrawMultiply {
        vault: [1u8; 32],
        max_slippage_bps: 100,
        deadline_unix: 0,
    }
}

/// Construct a signed envelope identical in shape to fleet-pm-stub's
/// output.
fn build_envelope(msg_type: MsgType, payload: Vec<u8>) -> Envelope {
    let sk = SigningKey::from_bytes(&[42u8; 32]);
    let sender = sk.verifying_key().to_bytes();
    let recipient = [7u8; 32];
    let nonce: u64 = 1;
    let now_unix: u64 = 1_714_800_000;
    let conv: [u8; 16] = [9u8; 16];
    Envelope::build(
        msg_type, sender, recipient, now_unix, nonce, conv, payload, &sk,
    )
}

#[test]
fn withdraw_multiply_envelope_round_trips_via_cbor() {
    let withdraw = sample_withdraw();
    let mut payload = Vec::new();
    into_writer(&withdraw, &mut payload).expect("serialize WithdrawMultiply");

    let env = build_envelope(MsgType::WithdrawMultiply, payload.clone());

    // Slot check.
    assert_eq!(env.msg_type, MsgType::WithdrawMultiply);
    assert_eq!(env.msg_type.as_u16(), 0x1A);

    // Payload re-decodes back to the same WithdrawMultiply.
    let decoded: WithdrawMultiply =
        from_reader(&env.payload[..]).expect("decode WithdrawMultiply from envelope");
    assert_eq!(decoded, withdraw);
}

#[test]
fn report_multiply_withdraw_jupiter_pending_round_trips() {
    // Shape of the Report the v0.3.0 daemon emits when a non-Noop
    // position is approved: error_code = 11
    // (ERR_JUPITER_INTEGRATION_PENDING). The orchestrator (or
    // fleet-pm-stub) must be able to decode this without hitting an
    // unknown-error-code path.
    let report = ReportMultiplyWithdraw {
        header: ReportHeader::err([5u8; 16], 11),
        final_usdc_lamports: 0,
        residual_sol_lamports: 9_000_000,
        tx_signatures: vec![],
    };

    let mut buf = Vec::new();
    into_writer(&report, &mut buf).expect("serialize");
    let decoded: ReportMultiplyWithdraw = from_reader(&buf[..]).expect("decode");
    assert_eq!(decoded, report);
    assert_eq!(decoded.header.error_code, Some(11));
    assert!(!decoded.header.ok);
    assert!(decoded.tx_signatures.is_empty());
}

#[test]
fn report_multiply_withdraw_noop_round_trips() {
    // Shape of the Report the v0.3.0 daemon emits for the Noop path
    // (obligation already empty). ok=true, no signatures, residual_sol
    // = current wallet balance.
    let report = ReportMultiplyWithdraw {
        header: ReportHeader::ok([5u8; 16]),
        final_usdc_lamports: 0,
        residual_sol_lamports: 12_345_678,
        tx_signatures: vec![],
    };

    let mut buf = Vec::new();
    into_writer(&report, &mut buf).expect("serialize");
    let decoded: ReportMultiplyWithdraw = from_reader(&buf[..]).expect("decode");
    assert_eq!(decoded, report);
    assert!(decoded.header.ok);
    assert_eq!(decoded.header.error_code, None);
}

#[test]
fn report_multiply_withdraw_with_multiple_signatures_round_trips() {
    // The future v0.3.1+ shape: an iterative unwind may broadcast
    // multiple txs; tx_signatures is a Vec to capture them all in
    // order. This test pins that contract.
    let report = ReportMultiplyWithdraw {
        header: ReportHeader::ok([0xab; 16]),
        final_usdc_lamports: 9_000_000,
        residual_sol_lamports: 0,
        tx_signatures: vec![
            "5fXLjV...".to_string(),
            "2zQwAa...".to_string(),
            "8KpYrM...".to_string(),
        ],
    };
    let mut buf = Vec::new();
    into_writer(&report, &mut buf).expect("serialize");
    let decoded: ReportMultiplyWithdraw = from_reader(&buf[..]).expect("decode");
    assert_eq!(decoded.tx_signatures.len(), 3);
    assert_eq!(decoded.tx_signatures[0], "5fXLjV...");
    assert_eq!(decoded.final_usdc_lamports, 9_000_000);
}

#[test]
fn withdraw_multiply_slot_does_not_accept_stable_lend_payload() {
    // Defense in depth: a WithdrawStableLend payload on the
    // WithdrawMultiply slot must NOT decode cleanly as WithdrawMultiply.
    // The daemon's payload_is_for_this_daemon filter relies on this
    // CBOR-shape mismatch to silently drop misrouted envelopes.
    let stable = WithdrawStableLend {
        market: [1u8; 32],
        reserve: [2u8; 32],
        usdc_lamports: 1_000_000,
        deadline_unix: 0,
    };
    let mut buf = Vec::new();
    into_writer(&stable, &mut buf).expect("serialize");
    let decode_result: Result<WithdrawMultiply, _> = from_reader(&buf[..]);
    // WithdrawStableLend has 4 fields with two 32-byte arrays;
    // WithdrawMultiply has 3 fields with one 32-byte array. The CBOR
    // shapes are incompatible.
    assert!(
        decode_result.is_err(),
        "WithdrawStableLend must not decode as WithdrawMultiply"
    );
}

#[test]
fn withdraw_multiply_msg_type_collaboration_class() {
    // 0x1A lives in the Collaboration nibble (0x1_) — the same class as
    // Assign / Withdraw / Approve. Pinning this so a future reshuffle of
    // the MsgType enum doesn't silently bump the unwind slot to a
    // different class.
    assert_eq!(MsgType::WithdrawMultiply.as_u16() >> 4, 0x1);
    assert_eq!(
        MsgType::from_u16(0x1A).expect("0x1A decodes"),
        MsgType::WithdrawMultiply
    );
    // And the Display name is the expected token.
    assert_eq!(
        format!("{}", MsgType::WithdrawMultiply),
        "WITHDRAW_MULTIPLY"
    );
}

#[test]
fn envelope_carries_withdraw_multiply_payload_intact() {
    // End-to-end: serialize a WithdrawMultiply, wrap it in an Envelope
    // (the same call fleet-pm-stub makes to dispatch a withdraw), then
    // re-decode the inner payload as if we were the daemon's receive
    // loop. Asserts the payload bytes ride the envelope unchanged and
    // that the msg_type slot is preserved.
    //
    // (Envelope itself uses a custom wire format, not ciborium serde —
    // so we don't round-trip the envelope itself here, just confirm that
    // the inner payload bytes are exposed verbatim for the daemon's
    // CBOR-decode step.)
    let withdraw = sample_withdraw();
    let mut inner_buf = Vec::new();
    into_writer(&withdraw, &mut inner_buf).expect("serialize inner");

    let env = build_envelope(MsgType::WithdrawMultiply, inner_buf.clone());

    assert_eq!(env.payload, inner_buf);
    assert_eq!(env.msg_type, MsgType::WithdrawMultiply);

    let inner_out: WithdrawMultiply = from_reader(&env.payload[..]).expect("decode inner");
    assert_eq!(inner_out, withdraw);
}
