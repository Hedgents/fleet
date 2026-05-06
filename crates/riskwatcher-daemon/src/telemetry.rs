//! M9: operational telemetry — JSONL poll log + Prometheus escalate counters.
//!
//! Two responsibilities, two types:
//!
//!   * [`TelemetryLog`] — append-only line-delimited JSON file. The poller
//!     writes one [`PollLogEntry`] per position per tick. Concurrent writes
//!     are serialised under a single `tokio::sync::Mutex<File>` so lines
//!     never interleave. Failures are surfaced to the caller (the poller
//!     downgrades them to a `warn!`; the daemon never dies on a telemetry
//!     write).
//!
//!   * [`EscalateMetrics`] — three `AtomicU64` counters partitioned by
//!     [`RiskSeverity`], rendered to the Prometheus text format on demand.
//!     The metrics endpoint task in `main.rs` formats and serves the
//!     output; the escalate emitter increments on each successful logical
//!     escalation (once per `(subject, severity)` even if the Critical
//!     fan-out targets two recipients).
//!
//! Both types live behind `Arc` and are cloned cheaply into the poller, the
//! escalate emitter, and the metrics HTTP task.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use tokio::sync::Mutex;

use zerox1_protocol::fleet::riskwatcher::RiskSeverity;

/// One line in the poll telemetry log. Serialised compactly (no pretty
/// printing — these files grow to 90k+ lines/day at default cadence).
///
/// `distance_bps` is `None` when the position has zero exposure
/// (`unhealthy_borrow_value_sf == 0`); `classification` is `None` when
/// the position is in no band (`distance > DISTANCE_NOTICE_BPS`).
#[derive(Debug, Clone, Serialize)]
pub struct PollLogEntry {
    pub ts: u64,
    /// Hex-encoded 32-byte agent_id of the multiply daemon that owns the
    /// position.
    pub subject: String,
    pub ltv_bps: u16,
    pub distance_bps: Option<u16>,
    /// Stable string variant name: `"Notice"`, `"Warning"`, `"Critical"`,
    /// or `None`. Explicit match (not `Debug`) so future enum changes
    /// can't silently break consumers.
    pub classification: Option<String>,
}

/// Map a [`RiskSeverity`] to its stable serialisation string. Pulled out
/// so test assertions can use the same source of truth as the writer.
pub fn severity_label(s: RiskSeverity) -> &'static str {
    match s {
        RiskSeverity::Notice => "Notice",
        RiskSeverity::Warning => "Warning",
        RiskSeverity::Critical => "Critical",
    }
}

/// Append-only JSONL writer. Held inside an `Arc<TelemetryLog>` so multiple
/// poll-tick tasks can hand off lines concurrently without interleaving.
///
/// We do NOT fsync per line — at 32 positions × 1 line per 30s that's ~92k
/// fsyncs per day on a hot disk, which kills SSD wear. The OS page cache
/// flushes naturally; the daemon's runbook spec accepts that the last
/// few seconds of telemetry may be lost on a hard kill.
pub struct TelemetryLog {
    file: Mutex<std::fs::File>,
    path: PathBuf,
}

impl TelemetryLog {
    /// Open (or create) a telemetry log at `path`. Parent directories are
    /// created best-effort; the open fails fast if the path itself is not
    /// writable, which is the right behaviour at boot — better to refuse
    /// to start than to silently drop telemetry.
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening telemetry log {}", path.display()))?;
        Ok(Self {
            file: Mutex::new(file),
            path,
        })
    }

    /// Serialise `entry` as one JSON object followed by `\n` and append.
    /// Returns an error on serialisation failure or write failure; the
    /// poller maps both to a `warn!` and continues.
    pub async fn write_line(&self, entry: &PollLogEntry) -> Result<()> {
        let mut buf = serde_json::to_vec(entry).context("serialize PollLogEntry")?;
        buf.push(b'\n');
        let mut guard = self.file.lock().await;
        guard.write_all(&buf).context("write telemetry line")?;
        Ok(())
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

/// Three Prometheus-text counters partitioned by [`RiskSeverity`].
///
/// Each counter is incremented exactly once per logical Escalate emission
/// (i.e. once per `(subject, severity)` pair on a dedup miss) — NOT once
/// per recipient envelope. The Critical fan-out to subject + orchestrator
/// is one logical event, so it bumps `critical` by one, not two.
#[derive(Default)]
pub struct EscalateMetrics {
    notice: AtomicU64,
    warning: AtomicU64,
    critical: AtomicU64,
}

impl EscalateMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bump the counter for `severity`. Relaxed ordering is correct: the
    /// only consumer is `snapshot()` for rendering, which has no
    /// cross-counter consistency requirement.
    pub fn inc(&self, severity: RiskSeverity) {
        match severity {
            RiskSeverity::Notice => {
                self.notice.fetch_add(1, Ordering::Relaxed);
            }
            RiskSeverity::Warning => {
                self.warning.fetch_add(1, Ordering::Relaxed);
            }
            RiskSeverity::Critical => {
                self.critical.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// `(notice, warning, critical)` counts at the moment of the call.
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.notice.load(Ordering::Relaxed),
            self.warning.load(Ordering::Relaxed),
            self.critical.load(Ordering::Relaxed),
        )
    }

    /// Render the Prometheus text-format snapshot. Stable enough that
    /// operator dashboards and grep-based alert rules can hard-code the
    /// label values (`severity="critical"` etc.).
    pub fn render_prometheus(&self) -> String {
        let (n, w, c) = self.snapshot();
        format!(
            "# HELP riskwatcher_escalates_total Total Escalate envelopes emitted by severity.\n\
             # TYPE riskwatcher_escalates_total counter\n\
             riskwatcher_escalates_total{{severity=\"notice\"}} {n}\n\
             riskwatcher_escalates_total{{severity=\"warning\"}} {w}\n\
             riskwatcher_escalates_total{{severity=\"critical\"}} {c}\n",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    /// Build a unique tempfile path for each test run. We avoid pulling in
    /// the `tempfile` crate (not in the workspace deps) — `std::env::temp_dir()`
    /// + nanos + pid is sufficient for unit-test isolation.
    fn temp_jsonl(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("riskwatcher-telemetry-{tag}-{pid}-{nanos}.jsonl"))
    }

    /// Round-trip three entries through `TelemetryLog::write_line` and
    /// parse them back. Asserts every field is preserved verbatim.
    #[tokio::test]
    async fn roundtrip_line() {
        let path = temp_jsonl("roundtrip");
        let log = TelemetryLog::open(path.clone()).expect("open telemetry log");

        let entries = vec![
            PollLogEntry {
                ts: 1_700_000_000,
                subject: "aa".repeat(32),
                ltv_bps: 4242,
                distance_bps: Some(150),
                classification: Some("Warning".to_string()),
            },
            PollLogEntry {
                ts: 1_700_000_030,
                subject: "bb".repeat(32),
                ltv_bps: 0,
                distance_bps: None,
                classification: None,
            },
            PollLogEntry {
                ts: 1_700_000_060,
                subject: "cc".repeat(32),
                ltv_bps: 9500,
                distance_bps: Some(10),
                classification: Some("Critical".to_string()),
            },
        ];

        for e in &entries {
            log.write_line(e).await.expect("write line");
        }
        // Drop the log so the file handle flushes (Mutex still held inside
        // `log` until drop; we want guaranteed-readable bytes on disk).
        drop(log);

        let f = std::fs::File::open(&path).expect("reopen for read");
        let lines: Vec<String> = BufReader::new(f)
            .lines()
            .collect::<std::io::Result<_>>()
            .expect("read lines");
        assert_eq!(lines.len(), 3, "exactly 3 lines written");

        for (raw, expected) in lines.iter().zip(entries.iter()) {
            let parsed: serde_json::Value = serde_json::from_str(raw).expect("valid JSON");
            assert_eq!(parsed["ts"], expected.ts);
            assert_eq!(parsed["subject"], expected.subject);
            assert_eq!(parsed["ltv_bps"], expected.ltv_bps);
            match expected.distance_bps {
                Some(b) => assert_eq!(parsed["distance_bps"], b),
                None => assert!(parsed["distance_bps"].is_null()),
            }
            match &expected.classification {
                Some(c) => assert_eq!(parsed["classification"], c.as_str()),
                None => assert!(parsed["classification"].is_null()),
            }
        }

        std::fs::remove_file(&path).ok();
    }

    /// Counter math: 3× Critical, 1× Warning, 0× Notice yields the
    /// (0, 1, 3) tuple the spec calls for.
    #[test]
    fn escalate_counter() {
        let m = EscalateMetrics::new();
        m.inc(RiskSeverity::Critical);
        m.inc(RiskSeverity::Critical);
        m.inc(RiskSeverity::Critical);
        m.inc(RiskSeverity::Warning);
        assert_eq!(m.snapshot(), (0, 1, 3));
    }

    /// Prometheus rendering must contain stable label/value pairs that
    /// operator dashboards can grep for.
    #[test]
    fn prometheus_render() {
        let m = EscalateMetrics::new();
        m.inc(RiskSeverity::Notice);
        m.inc(RiskSeverity::Notice);
        m.inc(RiskSeverity::Warning);
        m.inc(RiskSeverity::Critical);
        m.inc(RiskSeverity::Critical);
        m.inc(RiskSeverity::Critical);
        m.inc(RiskSeverity::Critical);

        let out = m.render_prometheus();
        assert!(
            out.contains("# TYPE riskwatcher_escalates_total counter"),
            "missing TYPE comment, got: {out}",
        );
        assert!(
            out.contains("riskwatcher_escalates_total{severity=\"notice\"} 2"),
            "missing notice=2, got: {out}",
        );
        assert!(
            out.contains("riskwatcher_escalates_total{severity=\"warning\"} 1"),
            "missing warning=1, got: {out}",
        );
        assert!(
            out.contains("riskwatcher_escalates_total{severity=\"critical\"} 4"),
            "missing critical=4, got: {out}",
        );
    }
}
