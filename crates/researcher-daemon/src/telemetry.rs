//! Telemetry log for emitted signals. Every MarketSignal that goes
//! out (via signal::emit_to / signal::emit_broadcast) appends a JSONL
//! line. A periodic task flushes a running-tally summary every hour
//! at INFO level.
//!
//! Pattern mirrors `stable-yield-daemon::telemetry` (M7) and
//! `multiply-daemon::pnl` — append-on-event JSONL plus interval flush.
//!
//! Single-writer file integrity is enforced by an `Mutex<()>` held
//! around the open/append/close cycle. We re-open per write rather
//! than holding a long-lived `File` so the file can be rotated by
//! external tooling without daemon restart.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::time::interval;
use tracing::info;

use zerox1_protocol::fleet::researcher::{MarketSignal, SignalSeverity};

/// Tracks per-window signal counts by severity for the running tally.
///
/// Three lock-free atomic counters; `record` is called from the emit
/// chokepoint on every signal that goes out. `drain` resets all three
/// and returns the previous values — called once per tally interval.
pub struct TelemetryTally {
    pub info: AtomicU32,
    pub notice: AtomicU32,
    pub important: AtomicU32,
}

impl TelemetryTally {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            info: AtomicU32::new(0),
            notice: AtomicU32::new(0),
            important: AtomicU32::new(0),
        })
    }

    pub fn record(&self, severity: SignalSeverity) {
        match severity {
            SignalSeverity::Info => self.info.fetch_add(1, Ordering::Relaxed),
            SignalSeverity::Notice => self.notice.fetch_add(1, Ordering::Relaxed),
            SignalSeverity::Important => self.important.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Reset all counts and return the previous values as
    /// `(info, notice, important)`.
    pub fn drain(&self) -> (u32, u32, u32) {
        (
            self.info.swap(0, Ordering::Relaxed),
            self.notice.swap(0, Ordering::Relaxed),
            self.important.swap(0, Ordering::Relaxed),
        )
    }

    pub fn total(&self) -> u32 {
        self.info.load(Ordering::Relaxed)
            + self.notice.load(Ordering::Relaxed)
            + self.important.load(Ordering::Relaxed)
    }
}

/// All state the emit chokepoint needs to record telemetry: the
/// JSONL log path, a single-writer mutex, and the tally counter.
///
/// Wrapped in `Arc<TelemetryHandle>` and threaded through every
/// watcher's `run(...)` so each emit can append + bump.
pub struct TelemetryHandle {
    pub log_path: PathBuf,
    pub log_writer: Mutex<()>,
    pub tally: Arc<TelemetryTally>,
}

impl TelemetryHandle {
    pub fn new(log_path: PathBuf, tally: Arc<TelemetryTally>) -> Arc<Self> {
        Arc::new(Self {
            log_path,
            log_writer: Mutex::new(()),
            tally,
        })
    }
}

#[derive(Serialize)]
struct SignalLine<'a> {
    ts: u64,
    kind: &'static str,
    asset: &'static str,
    asset_mint_hex: String,
    measurement_bps: i32,
    severity: &'a str,
    context_value: u64,
    recipient_count: usize,
}

/// Append a signal-emission record to the JSONL log + bump the tally.
/// Called from the signal emit chokepoint. Errors from disk I/O are
/// returned to the caller; callers in the emit path treat them as
/// non-fatal (warn + continue).
pub async fn record_emission(
    log_path: &PathBuf,
    log_writer: &Mutex<()>,
    tally: &TelemetryTally,
    signal: &MarketSignal,
    recipient_count: usize,
) -> Result<()> {
    tally.record(signal.severity);

    let line = SignalLine {
        ts: signal.raised_at_unix,
        kind: kind_str(signal.kind),
        asset: asset_str(signal.asset),
        asset_mint_hex: hex::encode(&signal.asset_mint[..8]),
        measurement_bps: signal.measurement_bps,
        severity: severity_str(signal.severity),
        context_value: signal.context_value,
        recipient_count,
    };
    let json = serde_json::to_string(&line).context("serialize SignalLine")?;

    let _g = log_writer.lock().await;
    // Audit-fix M1: create the JSONL log with mode 0600 on Unix so the
    // role keypair file (already 0600) and the telemetry log share the
    // same permission posture. Without this, OpenOptions defaults to
    // 0644 — readable by other local users.
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    // tokio::fs::OpenOptions exposes `mode` as an inherent method on Unix
    // — no extra trait import needed.
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }
    let mut f = opts
        .open(log_path)
        .await
        .context("open telemetry log")?;
    f.write_all(json.as_bytes()).await?;
    f.write_all(b"\n").await?;
    Ok(())
}

/// Periodic task that emits a running-tally summary every
/// `interval_secs`. Default interval is 3600 = 1h; tests can use a
/// shorter interval.
pub async fn run_tally_loop(tally: Arc<TelemetryTally>, interval_secs: u64) {
    let mut tick = interval(Duration::from_secs(interval_secs));
    tick.tick().await; // skip first immediate fire
    loop {
        tick.tick().await;
        let (info_n, notice_n, important_n) = tally.drain();
        let total = info_n + notice_n + important_n;
        info!(
            total,
            info = info_n,
            notice = notice_n,
            important = important_n,
            "researcher signal tally (rolling window)"
        );
    }
}

/// Stringify SignalKind for log lines.
fn kind_str(k: zerox1_protocol::fleet::researcher::SignalKind) -> &'static str {
    use zerox1_protocol::fleet::researcher::SignalKind::*;
    match k {
        LendingBorrowRateAbove => "LendingBorrowRateAbove",
        LendingSupplyRateAbove => "LendingSupplyRateAbove",
        PerpFundingAbove => "PerpFundingAbove",
        PerpFundingBelow => "PerpFundingBelow",
        PriceMovedBps => "PriceMovedBps",
        JlpYieldChanged => "JlpYieldChanged",
        JlpCompositionShifted => "JlpCompositionShifted",
        StableDepegBps => "StableDepegBps",
        NewTokenLaunched => "NewTokenLaunched",
        LargeTokenTrade => "LargeTokenTrade",
        Other => "Other",
    }
}

fn asset_str(a: zerox1_protocol::fleet::researcher::AssetId) -> &'static str {
    use zerox1_protocol::fleet::researcher::AssetId::*;
    match a {
        SOL => "SOL",
        ETH => "ETH",
        BTC => "BTC",
        USDC => "USDC",
        USDT => "USDT",
        JLP => "JLP",
        Other => "Other",
    }
}

fn severity_str(s: SignalSeverity) -> &'static str {
    match s {
        SignalSeverity::Info => "Info",
        SignalSeverity::Notice => "Notice",
        SignalSeverity::Important => "Important",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerox1_protocol::fleet::researcher::{AssetId, SignalKind};

    fn synthetic_signal(severity: SignalSeverity) -> MarketSignal {
        MarketSignal {
            kind: SignalKind::PerpFundingAbove,
            asset: AssetId::SOL,
            asset_mint: [0u8; 32],
            measurement_bps: 2500,
            severity,
            raised_at_unix: 1714000000,
            context_value: 0,
        }
    }

    #[test]
    fn tally_records_each_severity() {
        let t = TelemetryTally::new();
        t.record(SignalSeverity::Info);
        t.record(SignalSeverity::Info);
        t.record(SignalSeverity::Notice);
        t.record(SignalSeverity::Important);
        assert_eq!(t.total(), 4);
        let (i, n, p) = t.drain();
        assert_eq!((i, n, p), (2, 1, 1));
        assert_eq!(t.total(), 0);
    }

    #[tokio::test]
    async fn record_emission_appends_jsonl() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("rsr-m9-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let tally = TelemetryTally::new();
        let lock = Mutex::new(());

        let s = synthetic_signal(SignalSeverity::Notice);
        record_emission(&path, &lock, &tally, &s, 3).await.unwrap();
        record_emission(&path, &lock, &tally, &s, 2).await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        // Each line is valid JSON with the expected keys.
        for l in lines {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert_eq!(v["kind"], "PerpFundingAbove");
            assert_eq!(v["asset"], "SOL");
            assert_eq!(v["severity"], "Notice");
            assert_eq!(v["measurement_bps"], 2500);
        }
        // Tally also bumped.
        assert_eq!(tally.notice.load(Ordering::Relaxed), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn telemetry_file_is_mode_600() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir()
            .join(format!("rsr-mode-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let tally = TelemetryTally::new();
        let lock = Mutex::new(());
        let s = synthetic_signal(SignalSeverity::Notice);
        record_emission(&path, &lock, &tally, &s, 1).await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "telemetry log should be 0600, got {:o}",
            mode
        );
        let _ = std::fs::remove_file(&path);
    }
}
