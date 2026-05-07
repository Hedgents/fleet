//! Periodic telemetry. Polls JLP balance + pool state + per-custody
//! oracle prices + active hedge positions. Writes one JSONL line per
//! tick capturing the moment-by-moment net APR estimate.
//!
//! Operator-facing observability — this is what we look at when the
//! mainnet test is running. APR fields are v0 placeholders (zero) until
//! a JLP yield decoder + custody borrow-rate aggregation land. The
//! delta + notional fields are live the moment a position is recorded
//! into `RebalanceState`.
//!
//! Failure handling: every error path inside `poll_once` is non-fatal
//! and surfaces a sentinel zeros line — telemetry must never take down
//! the daemon. The outer `run` loop logs at WARN if `poll_once` itself
//! returns an `Err` (only the file-write paths can do so) and continues
//! ticking.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::interval;
use tracing::{debug, info, warn};

use zerox1_defi_runtime::rpc::RpcContext;

use crate::jlp_hedge::read_pool_state;
use crate::rebalance::RebalanceState;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TelemetryLine {
    pub ts: u64,
    pub jlp_lamports: u64,
    /// micro-USD ($1 = 1_000_000)
    pub jlp_value_usd_micro: u64,
    pub hedge_notional_usdc: u64,
    pub current_delta_bps: i16,
    pub long_exposure_bps: u16,
    /// Audit-fix I1: APR fields are `Option<i32>`. `None` (serialized
    /// as field-absent via `skip_serializing_if`) means "not measured"
    /// — operators reading the JSONL distinguish absent from zero. v0
    /// always writes `None` until the JLP-yield + custody borrow-rate
    /// decoders land. Deserializer accepts missing or null and resolves
    /// to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jlp_yield_apr_bps: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hedge_borrow_apr_bps: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_apr_bps: Option<i32>,
}

pub async fn run(
    rpc: Arc<RpcContext>,
    state: Arc<RebalanceState>,
    log_path: PathBuf,
    interval_secs: u64,
) {
    let mut tick = interval(Duration::from_secs(interval_secs.max(1)));
    info!(
        path = %log_path.display(),
        interval_secs,
        "hedgedjlp telemetry starting"
    );
    // First tick fires immediately — operator sees a line within
    // seconds of boot.
    tick.tick().await;
    if let Err(e) = poll_once(&rpc, &state, &log_path).await {
        warn!(?e, "telemetry poll failed (non-fatal)");
    }
    loop {
        tick.tick().await;
        if let Err(e) = poll_once(&rpc, &state, &log_path).await {
            warn!(?e, "telemetry poll failed (non-fatal)");
        }
    }
}

async fn poll_once(
    rpc: &Arc<RpcContext>,
    state: &Arc<RebalanceState>,
    log_path: &Path,
) -> Result<()> {
    let active = state.snapshot_active_position();

    let line = match active {
        Some(active) if !active.custody_pubkeys.is_empty() => {
            // Active position with custodies — try to read live state.
            // M9's `read_pool_state` does the heavy lifting.
            match read_pool_state(rpc, active.our_jlp_lamports, &active.custody_pubkeys).await {
                Ok((delta, _supply)) => TelemetryLine {
                    ts: now_unix(),
                    jlp_lamports: active.our_jlp_lamports,
                    jlp_value_usd_micro: delta.total_usd,
                    hedge_notional_usdc: active.hedge_notional_usdc,
                    current_delta_bps: active.target_delta_bps,
                    long_exposure_bps: delta.long_exposure_bps,
                    // Audit-fix I1: APR fields stay None until decoders
                    // land (JLP yield from pool, weighted-avg of
                    // custody borrow rates). Operators reading the
                    // JSONL see the field absent rather than zero.
                    jlp_yield_apr_bps: None,
                    hedge_borrow_apr_bps: None,
                    net_apr_bps: None,
                },
                Err(e) => {
                    warn!(
                        ?e,
                        "telemetry read_pool_state failed — falling back to active sentinel",
                    );
                    sentinel(
                        active.our_jlp_lamports,
                        active.hedge_notional_usdc,
                        active.target_delta_bps,
                    )
                }
            }
        }
        Some(active) => {
            // Active but no custody list (e.g. sim-only path) — write a
            // sentinel that still surfaces the recorded notional + target.
            sentinel(
                active.our_jlp_lamports,
                active.hedge_notional_usdc,
                active.target_delta_bps,
            )
        }
        None => {
            // No active position — write a sentinel zeros line so
            // operators see the daemon ticking.
            sentinel(0, 0, 0)
        }
    };

    append_line(log_path, &line).await?;
    debug!(
        jlp_lamports = line.jlp_lamports,
        jlp_value_usd_micro = line.jlp_value_usd_micro,
        hedge_notional_usdc = line.hedge_notional_usdc,
        long_exposure_bps = line.long_exposure_bps,
        "telemetry tick recorded",
    );
    Ok(())
}

fn sentinel(jlp: u64, hedge: u64, target_bps: i16) -> TelemetryLine {
    TelemetryLine {
        ts: now_unix(),
        jlp_lamports: jlp,
        jlp_value_usd_micro: 0,
        hedge_notional_usdc: hedge,
        current_delta_bps: target_bps,
        long_exposure_bps: 0,
        // Audit-fix I1: APR fields are None sentinels (skipped from
        // JSON when serialized).
        jlp_yield_apr_bps: None,
        hedge_borrow_apr_bps: None,
        net_apr_bps: None,
    }
}

async fn append_line(log_path: &Path, line: &TelemetryLine) -> Result<()> {
    use tokio::fs::OpenOptions;
    use tokio::io::AsyncWriteExt;

    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
    }

    let json = serde_json::to_string(line).context("serialize TelemetryLine")?;

    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    // `tokio::fs::OpenOptions::mode` is provided directly when
    // `target_family = "unix"`; no extra trait import needed.
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }
    let mut f = opts
        .open(log_path)
        .await
        .with_context(|| format!("open telemetry log at {}", log_path.display()))?;
    f.write_all(json.as_bytes()).await?;
    f.write_all(b"\n").await?;
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rebalance::RebalanceState;
    use solana_sdk::commitment_config::CommitmentConfig;

    fn unique_log_path(tag: &str) -> PathBuf {
        let unique = format!(
            "hedgedjlp-telemetry-test-{}-{}-{}.jsonl",
            tag,
            std::process::id(),
            now_unix(),
        );
        std::env::temp_dir().join(unique)
    }

    #[test]
    fn sentinel_round_trips_via_json() {
        let s = sentinel(123_456, 789_000, -250);
        // Pin the schema fields explicitly.
        assert_eq!(s.jlp_lamports, 123_456);
        assert_eq!(s.hedge_notional_usdc, 789_000);
        assert_eq!(s.current_delta_bps, -250);
        assert_eq!(s.jlp_value_usd_micro, 0);
        assert_eq!(s.long_exposure_bps, 0);
        // Audit-fix I1: APR fields are None.
        assert_eq!(s.jlp_yield_apr_bps, None);
        assert_eq!(s.hedge_borrow_apr_bps, None);
        assert_eq!(s.net_apr_bps, None);

        let json = serde_json::to_string(&s).unwrap();
        let back: TelemetryLine = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);

        // Audit-fix I1: serialized JSON omits the None APR fields.
        // Operators reading the JSONL see absence, not zero.
        assert!(
            !json.contains("jlp_yield_apr_bps"),
            "None must be skipped from JSON: {json}"
        );
        assert!(
            !json.contains("hedge_borrow_apr_bps"),
            "None must be skipped from JSON: {json}"
        );
        assert!(
            !json.contains("net_apr_bps"),
            "None must be skipped from JSON: {json}"
        );
    }

    #[test]
    fn telemetry_line_with_some_apr_serializes_as_present() {
        // Future-proof: when decoders land and fill the APR fields,
        // the JSON must include them.
        let mut s = sentinel(1, 2, 0);
        s.jlp_yield_apr_bps = Some(4500);
        s.hedge_borrow_apr_bps = Some(800);
        s.net_apr_bps = Some(3700);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"jlp_yield_apr_bps\":4500"));
        assert!(json.contains("\"hedge_borrow_apr_bps\":800"));
        assert!(json.contains("\"net_apr_bps\":3700"));
    }

    #[tokio::test]
    async fn append_line_creates_with_0600_perms_on_unix_and_appends() {
        let path = unique_log_path("append");
        let _ = std::fs::remove_file(&path);

        let line1 = TelemetryLine {
            ts: 100,
            jlp_lamports: 1,
            jlp_value_usd_micro: 0,
            hedge_notional_usdc: 0,
            current_delta_bps: 0,
            long_exposure_bps: 0,
            jlp_yield_apr_bps: None,
            hedge_borrow_apr_bps: None,
            net_apr_bps: None,
        };
        let line2 = TelemetryLine {
            ts: 200,
            jlp_lamports: 2,
            ..line1.clone()
        };
        append_line(&path, &line1).await.unwrap();
        append_line(&path, &line2).await.unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "two appends should yield two lines");
        assert!(lines[0].contains("\"ts\":100"));
        assert!(lines[1].contains("\"ts\":200"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            // Lower 9 bits encode rwxrwxrwx.
            assert_eq!(
                mode & 0o777,
                0o600,
                "telemetry log must be 0600 (operator-secrets adjacent)",
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn poll_once_with_no_active_position_writes_sentinel_zeros() {
        let path = unique_log_path("sentinel");
        let _ = std::fs::remove_file(&path);

        // Fresh state — no active position.
        let state = Arc::new(RebalanceState::new());
        // RPC is never hit on the no-active branch; pass a real (but
        // unreachable) URL so construction succeeds without I/O.
        let rpc = Arc::new(RpcContext::new(
            "http://127.0.0.1:1".to_string(),
            CommitmentConfig::confirmed(),
        ));

        poll_once(&rpc, &state, &path).await.unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1);
        let parsed: TelemetryLine = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed.jlp_lamports, 0);
        assert_eq!(parsed.jlp_value_usd_micro, 0);
        assert_eq!(parsed.hedge_notional_usdc, 0);
        assert_eq!(parsed.current_delta_bps, 0);
        assert_eq!(parsed.long_exposure_bps, 0);

        let _ = std::fs::remove_file(&path);
    }
}
