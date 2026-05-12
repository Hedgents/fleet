//! End-to-end test: signal emission flow exercising dedup, severity
//! escalation, and telemetry recording in concert. No real network or
//! libp2p — just the EmissionTracker → record_emission → JSONL chain.

use std::sync::Arc;
use tokio::sync::Mutex;

use researcher_daemon::dedup::EmissionTracker;
use researcher_daemon::telemetry::{record_emission, TelemetryHandle, TelemetryTally};
use std::time::Duration;

use zerox1_protocol::fleet::researcher::{AssetId, MarketSignal, SignalKind, SignalSeverity};

fn synthetic(kind: SignalKind, asset: AssetId, severity: SignalSeverity) -> MarketSignal {
    MarketSignal {
        kind,
        asset,
        asset_mint: [0u8; 32],
        measurement_bps: 0,
        severity,
        raised_at_unix: 0,
        context_value: 0,
    }
}

async fn make_telemetry(label: &str) -> Arc<TelemetryHandle> {
    let path = std::env::temp_dir().join(format!(
        "rsr-m10-{}-{}-{}.jsonl",
        std::process::id(),
        label,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    Arc::new(TelemetryHandle {
        log_path: path,
        log_writer: Mutex::new(()),
        tally: TelemetryTally::new(),
    })
}

#[tokio::test]
async fn dedup_throttles_within_cooldown() {
    let tracker = EmissionTracker::new(Duration::from_secs(60));
    let tel = make_telemetry("dedup").await;

    let s = synthetic(
        SignalKind::LendingBorrowRateAbove,
        AssetId::SOL,
        SignalSeverity::Notice,
    );

    // Three attempts within cooldown — only first should pass.
    let mut emitted = 0usize;
    for _ in 0..3 {
        if tracker.should_emit(s.kind, s.asset, s.severity) {
            record_emission(&tel.log_path, &tel.log_writer, &tel.tally, &s, 1)
                .await
                .unwrap();
            emitted += 1;
        }
    }

    assert_eq!(emitted, 1);
    assert_eq!(
        tel.tally.notice.load(std::sync::atomic::Ordering::Relaxed),
        1
    );

    let content = tokio::fs::read_to_string(&tel.log_path).await.unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 1);

    let _ = std::fs::remove_file(&tel.log_path);
}

#[tokio::test]
async fn severity_escalation_overrides_dedup() {
    let tracker = EmissionTracker::new(Duration::from_secs(60));
    let tel = make_telemetry("escalation").await;

    let n = synthetic(
        SignalKind::StableDepegBps,
        AssetId::USDC,
        SignalSeverity::Notice,
    );
    let i = synthetic(
        SignalKind::StableDepegBps,
        AssetId::USDC,
        SignalSeverity::Important,
    );

    // Notice — passes (first ever)
    assert!(tracker.should_emit(n.kind, n.asset, n.severity));
    record_emission(&tel.log_path, &tel.log_writer, &tel.tally, &n, 1)
        .await
        .unwrap();

    // Same Notice within cooldown — throttled
    assert!(!tracker.should_emit(n.kind, n.asset, n.severity));

    // Important — escalation, passes despite cooldown
    assert!(tracker.should_emit(i.kind, i.asset, i.severity));
    record_emission(&tel.log_path, &tel.log_writer, &tel.tally, &i, 1)
        .await
        .unwrap();

    // Going BACK to Notice within cooldown — throttled (severity went down)
    assert!(!tracker.should_emit(n.kind, n.asset, n.severity));

    let content = tokio::fs::read_to_string(&tel.log_path).await.unwrap();
    assert_eq!(content.lines().count(), 2);
    assert_eq!(
        tel.tally.notice.load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(
        tel.tally
            .important
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );

    let _ = std::fs::remove_file(&tel.log_path);
}

#[tokio::test]
async fn different_kind_asset_tuples_isolated() {
    let tracker = EmissionTracker::new(Duration::from_secs(60));
    let tel = make_telemetry("isolated").await;

    let signals = vec![
        synthetic(
            SignalKind::LendingBorrowRateAbove,
            AssetId::SOL,
            SignalSeverity::Notice,
        ),
        synthetic(
            SignalKind::LendingBorrowRateAbove,
            AssetId::ETH,
            SignalSeverity::Notice,
        ),
        synthetic(
            SignalKind::LendingBorrowRateAbove,
            AssetId::BTC,
            SignalSeverity::Notice,
        ),
        synthetic(
            SignalKind::PriceMovedBps,
            AssetId::SOL,
            SignalSeverity::Notice,
        ),
        synthetic(
            SignalKind::StableDepegBps,
            AssetId::USDC,
            SignalSeverity::Notice,
        ),
    ];

    let mut emitted = 0usize;
    for s in &signals {
        if tracker.should_emit(s.kind, s.asset, s.severity) {
            record_emission(&tel.log_path, &tel.log_writer, &tel.tally, s, 1)
                .await
                .unwrap();
            emitted += 1;
        }
    }

    assert_eq!(emitted, 5);
    let content = tokio::fs::read_to_string(&tel.log_path).await.unwrap();
    assert_eq!(content.lines().count(), 5);
    assert_eq!(
        tel.tally.notice.load(std::sync::atomic::Ordering::Relaxed),
        5
    );

    let _ = std::fs::remove_file(&tel.log_path);
}
