//! Decoder unit tests — happy path for each msg_type the demo cares
//! about, plus the unmatched fall-through.

use fleet_dashboard_server::ingest::envelope_decoder::decode_log_line;
use fleet_dashboard_server::types::{Direction, RawLogLine};
use serde_json::json;
use std::path::PathBuf;

fn raw(target: &str, _message: &str, fields: serde_json::Value) -> RawLogLine {
    RawLogLine {
        source_file: PathBuf::from("test.log"),
        raw: json!({
            "timestamp": "2026-05-06T14:23:07.123Z",
            "level": "INFO",
            "target": target,
            "fields": fields,
        }),
    }
}

#[test]
fn beacon_event_decodes() {
    let r = raw(
        "hedgedjlp_daemon::main",
        "BEACON emitted",
        json!({"message": "BEACON emitted", "role": "hedgedjlp", "nonce": 42}),
    );
    let event = decode_log_line(&r).expect("should decode");
    assert_eq!(event.sender_role, "hedgedjlp");
    assert_eq!(event.msg_type, "Beacon");
    assert_eq!(event.direction, Direction::Out);
    assert!(event.payload_summary.contains("hedgedjlp"));
    assert!(event.payload_summary.contains("nonce 42") || event.payload_summary.contains("42"));
}

#[test]
fn assign_multiply_decodes() {
    let r = raw(
        "multiply_daemon::dispatch",
        "AssignMultiply received",
        json!({
            "message": "AssignMultiply received",
            "target_ltv_bps": 7000_i64,
            "max_slippage_bps": 50_i64,
        }),
    );
    let event = decode_log_line(&r).expect("should decode");
    assert_eq!(event.sender_role, "multiply");
    assert_eq!(event.msg_type, "Assign");
    assert_eq!(event.direction, Direction::In);
    assert!(event.payload_summary.contains("7000"));
}

#[test]
fn assign_hedgedjlp_decodes() {
    let r = raw(
        "hedgedjlp_daemon::dispatch",
        "AssignHedgedJlp received",
        json!({
            "message": "AssignHedgedJlp received",
            "usdc_lamports": 200_000_000_u64,
            "target_delta_bps": 0_i64,
            "max_borrow_rate_bps": 5000_u64,
            "deadline_unix": 0_u64,
        }),
    );
    let event = decode_log_line(&r).expect("should decode");
    assert_eq!(event.sender_role, "hedgedjlp");
    assert_eq!(event.msg_type, "Assign");
    assert_eq!(event.direction, Direction::In);
    assert!(event.payload_summary.contains("$200"));
}

#[test]
fn report_sent_decodes() {
    let r = raw(
        "multiply_daemon::dispatch",
        "report sent",
        json!({
            "message": "report sent",
            "ok": true,
            "conv": "[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]",
        }),
    );
    let event = decode_log_line(&r).expect("should decode");
    assert_eq!(event.msg_type, "Report");
    assert_eq!(event.direction, Direction::Out);
    assert!(event.payload_summary.contains("ok"));
}

#[test]
fn escalate_risk_decodes() {
    let r = raw(
        "riskwatcher_daemon::escalate",
        "EscalateRisk emitted",
        json!({
            "message": "EscalateRisk emitted",
            "severity": "Critical",
            "kind": "LiquidationDistance",
        }),
    );
    let event = decode_log_line(&r).expect("should decode");
    assert_eq!(event.msg_type, "Escalate");
    assert!(event.payload_summary.contains("Critical"));
}

#[test]
fn market_signal_decodes() {
    let r = raw(
        "researcher_daemon::watchers::price",
        "MarketSignal emitted",
        json!({
            "message": "MarketSignal emitted",
            "kind": "PriceMovedBps",
            "asset": "SOL",
            "severity": "Notice",
            "measurement_bps": 230_i64,
        }),
    );
    let event = decode_log_line(&r).expect("should decode");
    assert_eq!(event.sender_role, "researcher");
    assert_eq!(event.msg_type, "MarketSignal");
    assert!(event.payload_summary.to_lowercase().contains("sol"));
}

#[test]
fn approve_rejected_decodes() {
    let r = raw(
        "multiply_daemon::dispatch",
        "Approve REJECTED",
        json!({
            "message": "Approve REJECTED",
        }),
    );
    let event = decode_log_line(&r).expect("should decode");
    assert_eq!(event.msg_type, "Internal");
    assert_eq!(event.direction, Direction::Internal);
    assert!(event.payload_summary.contains("REJECTED"));
}

#[test]
fn rebalance_tick_decodes() {
    let r = raw(
        "hedgedjlp_daemon::rebalance",
        "rebalance tick",
        json!({
            "message": "rebalance tick",
            "note": "delta within band",
        }),
    );
    let event = decode_log_line(&r).expect("should decode");
    assert_eq!(event.msg_type, "Internal");
    assert_eq!(event.direction, Direction::Internal);
    assert_eq!(event.sender_role, "hedgedjlp");
    assert!(event.payload_summary.contains("delta within band"));
}

#[test]
fn unmatched_event_returns_none() {
    let r = raw(
        "multiply_daemon::leverage",
        "leverage round complete",
        json!({"message": "leverage round complete"}),
    );
    assert!(decode_log_line(&r).is_none());
}
