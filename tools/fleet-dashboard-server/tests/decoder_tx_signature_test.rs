//! Coverage for the on-chain tx_signature extraction path of
//! `envelope_decoder::decode_log_line`.
//!
//! Each daemon emits an `info!` line with `fields.sig = "<base58 sig>"`
//! when an on-chain ixn confirms. Pre-fleet-v0.2.8 the decoder only
//! matched stable-yield's "deposit confirmed on-chain"; the other four
//! patterns fell on the floor and `/onchain/activity` returned empty.
//!
//! These tests pin one pattern per daemon so a future log-message rename
//! shows up as a unit-test failure rather than a silent dashboard
//! regression.

use fleet_dashboard_server::ingest::envelope_decoder::decode_log_line;
use fleet_dashboard_server::types::RawLogLine;
use serde_json::json;
use std::path::PathBuf;

fn raw(target: &str, message: &str, extra: serde_json::Value) -> RawLogLine {
    let mut fields = serde_json::Map::new();
    fields.insert("message".to_string(), json!(message));
    if let Some(obj) = extra.as_object() {
        for (k, v) in obj {
            fields.insert(k.clone(), v.clone());
        }
    }
    let line = json!({
        "timestamp": "2026-05-13T12:00:00.000Z",
        "level": "INFO",
        "target": target,
        "fields": serde_json::Value::Object(fields),
    });
    RawLogLine {
        source_file: PathBuf::from("test.log"),
        raw: line,
    }
}

#[test]
fn stable_yield_deposit_confirmed_populates_tx_signature() {
    let line = raw(
        "stable_yield_daemon::lend",
        "deposit confirmed on-chain",
        json!({
            "sig": "5xS1gAtURGAvP9V8oRoT3vKqkLfg4dXP3Q5HzqGwQqEHc4Wb6Sjj1QwiTjJDfg",
            "apr_bps": 530,
            "conv": "abcd1234",
        }),
    );
    let ev = decode_log_line(&line).expect("should decode");
    assert_eq!(ev.sender_role, "stable_yield");
    assert_eq!(ev.msg_type, "OnChain");
    assert_eq!(
        ev.tx_signature.as_deref(),
        Some("5xS1gAtURGAvP9V8oRoT3vKqkLfg4dXP3Q5HzqGwQqEHc4Wb6Sjj1QwiTjJDfg")
    );
}

#[test]
fn multiply_seed_committed_populates_tx_signature() {
    let line = raw(
        "multiply_daemon::seed",
        "seed committed",
        json!({"sig": "SeedSigBASE58XXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"}),
    );
    let ev = decode_log_line(&line).expect("should decode");
    assert_eq!(ev.sender_role, "multiply");
    assert_eq!(ev.msg_type, "OnChain");
    assert_eq!(
        ev.tx_signature.as_deref(),
        Some("SeedSigBASE58XXXXXXXXXXXXXXXXXXXXXXXXXXXXXX")
    );
}

#[test]
fn multiply_round_committed_with_sig_populates_tx_signature() {
    let line = raw(
        "multiply_daemon::leverage",
        "round committed",
        json!({"sig": "RoundSigBASE58XXXXXXXXXXXXXXXXXXXXXXXXXXXX", "ix_count": 4}),
    );
    let ev = decode_log_line(&line).expect("should decode");
    assert_eq!(ev.sender_role, "multiply");
    assert_eq!(ev.msg_type, "OnChain");
    assert_eq!(
        ev.tx_signature.as_deref(),
        Some("RoundSigBASE58XXXXXXXXXXXXXXXXXXXXXXXXXXXX")
    );
}

#[test]
fn multiply_round_committed_without_sig_still_decodes() {
    // The first "round committed" log emitted by leverage.rs has no
    // `sig` field (it's a per-round bookkeeping line, not a confirmation).
    // We still want a MeshEvent row so the activity feed shows the round
    // boundary; tx_signature is just None.
    let line = raw(
        "multiply_daemon::leverage",
        "round committed",
        json!({"round": 2, "current_ltv_bps": 6500}),
    );
    let ev = decode_log_line(&line).expect("should decode");
    assert_eq!(ev.sender_role, "multiply");
    assert!(ev.tx_signature.is_none());
}

#[test]
fn hedgedjlp_jlp_buy_confirmed_populates_tx_signature() {
    let line = raw(
        "hedgedjlp_daemon::jlp_hedge",
        "JLP buy confirmed on-chain (via Jupiter)",
        json!({
            "sig": "JlpBuySigBASE58XXXXXXXXXXXXXXXXXXXXXXXXXXXX",
            "conv": "deadbeef",
        }),
    );
    let ev = decode_log_line(&line).expect("should decode");
    assert_eq!(ev.sender_role, "hedgedjlp");
    assert_eq!(ev.msg_type, "OnChain");
    assert_eq!(
        ev.tx_signature.as_deref(),
        Some("JlpBuySigBASE58XXXXXXXXXXXXXXXXXXXXXXXXXXXX")
    );
}

#[test]
fn hedgedjlp_short_open_submitted_populates_tx_signature() {
    let line = raw(
        "hedgedjlp_daemon::hedge",
        "hedge short-open request submitted",
        json!({
            "sig": "ShortOpenSigBASE58XXXXXXXXXXXXXXXXXXXXXXXX",
            "asset": "SOL",
        }),
    );
    let ev = decode_log_line(&line).expect("should decode");
    assert_eq!(ev.sender_role, "hedgedjlp");
    assert_eq!(ev.msg_type, "OnChain");
    assert!(ev.payload_summary.contains("SOL"));
    assert_eq!(
        ev.tx_signature.as_deref(),
        Some("ShortOpenSigBASE58XXXXXXXXXXXXXXXXXXXXXXXX")
    );
}

#[test]
fn tx_signature_falls_back_through_sig_tx_tx_signature() {
    // Legacy emitters use `tx`/`tx_signature` instead of `sig`. All three
    // shapes must round-trip through the decoder.
    for field in ["sig", "tx", "tx_signature"] {
        let line = raw(
            "stable_yield_daemon::lend",
            "deposit confirmed on-chain",
            json!({field: format!("VALUE-{field}")}),
        );
        let ev = decode_log_line(&line).expect("should decode");
        assert_eq!(
            ev.tx_signature.as_deref(),
            Some(format!("VALUE-{field}").as_str()),
            "field={field} should be picked up as tx_signature",
        );
    }
}

#[test]
fn last_sig_for_role_is_most_recent_per_strategy() {
    use fleet_dashboard_server::store::Store;
    use fleet_dashboard_server::types::{Direction, MeshEvent};

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let path = std::env::temp_dir().join(format!(
            "fds-last-sig-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).await.unwrap();

        let mk = |role: &str, ts_ms: i64, sig: Option<&str>| MeshEvent {
            id: None,
            ts_unix: ts_ms / 1000,
            ts_ms,
            sender_role: role.to_string(),
            direction: Direction::Out,
            msg_type: "OnChain".to_string(),
            payload_summary: "x".to_string(),
            payload_json: None,
            conv_id: None,
            tx_signature: sig.map(|s| s.to_string()),
        };
        // Insert two for each strategy with intentionally out-of-order
        // timestamps; last_sig_for_role must pick the newest by ts_ms.
        for ev in [
            mk("multiply", 1_000, Some("MUL_OLD")),
            mk("multiply", 3_000, Some("MUL_NEW")),
            mk("stable_yield", 2_000, Some("SY_OLD")),
            mk("stable_yield", 4_000, Some("SY_NEW")),
            mk("hedgedjlp", 1_500, Some("HJ_OLD")),
            mk("hedgedjlp", 5_000, Some("HJ_NEW")),
            // Null-sig rows must be ignored.
            mk("multiply", 9_999, None),
        ] {
            store.insert_mesh_event(&ev).await.unwrap();
        }

        assert_eq!(
            store
                .last_sig_for_role("multiply")
                .await
                .unwrap()
                .as_deref(),
            Some("MUL_NEW"),
        );
        assert_eq!(
            store
                .last_sig_for_role("stable_yield")
                .await
                .unwrap()
                .as_deref(),
            Some("SY_NEW"),
        );
        assert_eq!(
            store
                .last_sig_for_role("hedgedjlp")
                .await
                .unwrap()
                .as_deref(),
            Some("HJ_NEW"),
        );
        assert_eq!(store.last_sig_for_role("riskwatcher").await.unwrap(), None,);

        let _ = std::fs::remove_file(&path);
    });
}

#[test]
fn daemon_for_path_matches_live_pnl_variants() {
    use fleet_dashboard_server::ingest::pnl_jsonl::daemon_for_path;
    use std::path::Path;

    // Paper-trade / systemd-unit names (pre-fleet-v0.2.8).
    assert_eq!(
        daemon_for_path(Path::new("/var/lib/hedgents/logs/multiply-pnl.jsonl")),
        Some("multiply")
    );
    assert_eq!(
        daemon_for_path(Path::new("/var/lib/hedgents/logs/stable-yield-pnl.jsonl")),
        Some("stable_yield")
    );
    assert_eq!(
        daemon_for_path(Path::new("/var/lib/hedgents/logs/hedgedjlp-pnl.jsonl")),
        Some("hedgedjlp")
    );

    // Live-daemon variants (added in fleet-v0.2.8).
    assert_eq!(
        daemon_for_path(Path::new("/var/lib/hedgents/logs/multiply-live-pnl.jsonl")),
        Some("multiply")
    );
    assert_eq!(
        daemon_for_path(Path::new(
            "/var/lib/hedgents/logs/stable-yield-live-pnl.jsonl"
        )),
        Some("stable_yield")
    );
    assert_eq!(
        daemon_for_path(Path::new("/var/lib/hedgents/logs/hedgedjlp-live-pnl.jsonl")),
        Some("hedgedjlp")
    );

    // Unrelated files are still ignored.
    assert_eq!(
        daemon_for_path(Path::new("/var/lib/hedgents/logs/multiply.log")),
        None
    );
    assert_eq!(daemon_for_path(Path::new("/tmp/random.jsonl")), None);
}
